#!/usr/bin/env bash
# provision.sh — idempotent provisioning of Vanity Manager infrastructure.
#
# Provisions:
#   - Linode Object Storage bucket + access keys (S3-compatible source of truth)
#   - Akamai EdgeKV namespace + group (serving plane for EdgeWorker)
#
# Requires:
#   - linode-cli installed and authenticated (~/.config/linode-cli)
#   - akamai CLI installed with edgekv package (~/.edgerc, default section)
#   - jq for JSON parsing
#
# Emits: provision.env (gitignored) with all config values for `make run`/deploy.
# Safe to re-run — existing resources are detected and reused.

set -euo pipefail

cd "$(dirname "$0")/.."

PROJECT_NAME="${PROJECT_NAME:-vanity-manager}"
LINODE_REGION="${LINODE_REGION:-us-ord-1}"
LINODE_PROFILE="${LINODE_PROFILE:-}"   # e.g. presales-lz; blank = default
EDGERC_SECTION="${EDGERC_SECTION:-default}"
EKV_NETWORK="${EKV_NETWORK:-staging}"
EKV_GROUP="${EKV_GROUP:-redirects}"
# Akamai Authentication Group ID for the EdgeKV namespace. Must be numeric.
# Default: 230602 ("Compute"), which matches the demo center/compute folder.
EKV_AUTH_GROUP="${EKV_AUTH_GROUP:-230602}"
BUCKET="${BUCKET:-${PROJECT_NAME}-${LINODE_PROFILE:-$(whoami | tr '[:upper:]' '[:lower:]')}}"

LCLI=(linode-cli)
[ -n "$LINODE_PROFILE" ] && LCLI+=(--as-user "$LINODE_PROFILE")

echo "vanity-manager provision"
echo "  project:        $PROJECT_NAME"
echo "  linode profile: ${LINODE_PROFILE:-<default>}"
echo "  linode region:  $LINODE_REGION"
echo "  s3 bucket:      $BUCKET"
echo "  edgerc section: $EDGERC_SECTION"
echo "  ekv network:    $EKV_NETWORK"
echo "  ekv namespace:  $PROJECT_NAME"
echo "  ekv group:      $EKV_GROUP"
echo

# ---------- prerequisite checks ----------
need() {
  command -v "$1" >/dev/null 2>&1 || { echo "missing: $1"; exit 1; }
}
need linode-cli
need akamai
need jq
need curl

akamai edgekv --help >/dev/null 2>&1 || {
  echo "missing: akamai edgekv package — install with: akamai install edgekv"
  exit 1
}

# ---------- Object Storage access key (before bucket create — need creds to PUT bucket) ----------
echo "==> Object Storage access keys"
KEY_LABEL="${PROJECT_NAME}-$(date +%Y%m%d)"
EXISTING_KEY_JSON=$("${LCLI[@]}" --json object-storage keys-list 2>/dev/null || echo '[]')
EXISTING_ID=$(echo "$EXISTING_KEY_JSON" | jq -r --arg label "$PROJECT_NAME" \
  '[.[] | select((.label // "") | startswith($label))][0].id // empty')

if [ -n "$EXISTING_ID" ] && [ -f provision.env ] && grep -q '^S3_ACCESS_KEY=' provision.env; then
  echo "    reusing key id=$EXISTING_ID from provision.env"
  S3_ACCESS_KEY=$(grep '^S3_ACCESS_KEY=' provision.env | cut -d= -f2-)
  S3_SECRET_KEY=$(grep '^S3_SECRET_KEY=' provision.env | cut -d= -f2-)
else
  echo "    creating new unrestricted Object Storage key '$KEY_LABEL'"
  KEY_JSON=$("${LCLI[@]}" --json object-storage keys-create --label "$KEY_LABEL")
  S3_ACCESS_KEY=$(echo "$KEY_JSON" | jq -r '.[0].access_key // .access_key')
  S3_SECRET_KEY=$(echo "$KEY_JSON" | jq -r '.[0].secret_key // .secret_key')
fi

# Resolve the Linode E3 S3 endpoint for the chosen region.
S3_ENDPOINT="https://${LINODE_REGION/-1/}-10.linodeobjects.com"
# Strip region suffix variations: us-ord-1 → us-ord, then prepend us-ord-10.
case "$LINODE_REGION" in
  us-ord-1|us-ord) S3_ENDPOINT="https://us-ord-10.linodeobjects.com" ; S3_REGION="us-ord-1" ;;
  *) S3_ENDPOINT="https://${LINODE_REGION}.linodeobjects.com" ; S3_REGION="$LINODE_REGION" ;;
esac
echo "    endpoint: $S3_ENDPOINT  region: $S3_REGION"

# ---------- Object Storage bucket (via boto3 — linode-cli obj plugin has a botocore version bug) ----------
echo "==> Linode Object Storage bucket"
python3 - "$S3_ENDPOINT" "$S3_REGION" "$BUCKET" "$S3_ACCESS_KEY" "$S3_SECRET_KEY" <<'PY'
import sys, boto3, botocore
endpoint, region, bucket, ak, sk = sys.argv[1:6]
s3 = boto3.client(
    "s3",
    endpoint_url=endpoint,
    region_name=region,
    aws_access_key_id=ak,
    aws_secret_access_key=sk,
    config=boto3.session.Config(signature_version="s3v4", s3={"addressing_style":"path"}),
)
try:
    s3.head_bucket(Bucket=bucket)
    print(f"    bucket {bucket} already exists")
except botocore.exceptions.ClientError as e:
    code = e.response.get("Error", {}).get("Code", "")
    if code in ("404", "NoSuchBucket", "NotFound"):
        print(f"    creating bucket {bucket}")
        s3.create_bucket(Bucket=bucket)
    else:
        raise
PY

echo "    endpoint resolved above"

# (key creation moved above, before bucket create)

# ---------- EdgeKV namespace ----------
echo "==> Akamai EdgeKV namespace ($EKV_NETWORK)"
AKAMAI_EDGERC_ARGS=()
[ "$EDGERC_SECTION" != "default" ] && AKAMAI_EDGERC_ARGS+=(--section "$EDGERC_SECTION")
if akamai edgekv list ns "$EKV_NETWORK" "${AKAMAI_EDGERC_ARGS[@]}" 2>/dev/null \
    | grep -qE "\b$PROJECT_NAME\b"; then
  echo "    namespace $PROJECT_NAME exists"
else
  echo "    creating namespace $PROJECT_NAME in auth group $EKV_AUTH_GROUP"
  akamai edgekv create ns "$EKV_NETWORK" "$PROJECT_NAME" \
    --retention 0 \
    --geoLocation US \
    --groupId "$EKV_AUTH_GROUP" \
    "${AKAMAI_EDGERC_ARGS[@]}" || {
      echo "    FAILED: inspect with 'akamai edgekv create ns --help' and override EKV_AUTH_GROUP if needed"
      exit 1
    }
fi

# ---------- EdgeKV group (data bucket within the namespace) ----------
echo "==> Akamai EdgeKV group"
# A group is created implicitly on first write in most SDK versions; some
# CLI builds require an explicit create. Try create, ignore "already exists".
akamai edgekv write text "$EKV_NETWORK" "$PROJECT_NAME" "$EKV_GROUP" "__bootstrap__" "ok" \
  "${AKAMAI_EDGERC_ARGS[@]}" >/dev/null 2>&1 || true
akamai edgekv delete item "$EKV_NETWORK" "$PROJECT_NAME" "$EKV_GROUP" "__bootstrap__" \
  "${AKAMAI_EDGERC_ARGS[@]}" >/dev/null 2>&1 || true
echo "    group $EKV_GROUP ready"

# ---------- Akamai API credentials for EdgeKV writes ----------
# Read the EdgeGrid credentials from the default section of ~/.edgerc so
# the Spin admin-api can push to EdgeKV using the same identity we used above.
echo "==> Akamai EdgeGrid credentials (from ~/.edgerc [default])"
need awk
# Split only on the FIRST '=' so base64 secrets with trailing '=' padding
# survive intact. Using awk index/substr instead of -F regex.
eg_read() {
  awk -v s="[$EDGERC_SECTION]" -v k="$1" '
    $0==s {f=1; next}
    /^\[/ {f=0}
    f && index($0, k "=") == 1 {
      i = index($0, "="); v = substr($0, i+1)
      sub(/^ */, "", v); sub(/ *$/, "", v); sub(/\r$/, "", v)
      print v; exit
    }
    f && index($0, k " ") == 1 {
      i = index($0, "="); v = substr($0, i+1)
      sub(/^ */, "", v); sub(/ *$/, "", v); sub(/\r$/, "", v)
      print v; exit
    }
  ' ~/.edgerc
}
AKAMAI_HOST=$(eg_read host)
AKAMAI_CLIENT_TOKEN=$(eg_read client_token)
AKAMAI_CLIENT_SECRET=$(eg_read client_secret)
AKAMAI_ACCESS_TOKEN=$(eg_read access_token)
if [ -z "$AKAMAI_HOST" ]; then
  echo "    WARN: could not read [default] from ~/.edgerc — EKV writes will fail until set"
fi

# ---------- provision.env ----------
echo "==> writing provision.env"
cat > provision.env <<EOF
# Generated by scripts/provision.sh on $(date -Iseconds)
# DO NOT commit — gitignored.

S3_ENDPOINT=$S3_ENDPOINT
S3_REGION=$S3_REGION
S3_BUCKET=$BUCKET
LINODE_PROFILE=$LINODE_PROFILE
S3_ACCESS_KEY=$S3_ACCESS_KEY
S3_SECRET_KEY=$S3_SECRET_KEY

AKAMAI_HOST=$AKAMAI_HOST
AKAMAI_CLIENT_TOKEN=$AKAMAI_CLIENT_TOKEN
AKAMAI_CLIENT_SECRET=$AKAMAI_CLIENT_SECRET
AKAMAI_ACCESS_TOKEN=$AKAMAI_ACCESS_TOKEN
EKV_NETWORK=$EKV_NETWORK
EKV_NAMESPACE=$PROJECT_NAME
EKV_GROUP=$EKV_GROUP
EOF
chmod 600 provision.env

echo
echo "provision complete. Source provision.env before running \`spin up\` or \`make deploy\`:"
echo "  set -a; . ./provision.env; set +a"
