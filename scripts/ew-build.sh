#!/usr/bin/env bash
# ew-build.sh — package the EdgeWorker as a tgz ready for Akamai upload.
# Usage: scripts/ew-build.sh <version>

set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="${1:-$(git describe --always --dirty 2>/dev/null || echo dev)}"
OUT_DIR="dist"
NAMESPACE_OVERRIDE="${EW_NAMESPACE:-}"
GROUP_OVERRIDE="${EW_GROUP:-}"
BUNDLE_NAME="${EW_BUNDLE_NAME:-vanity-manager}"
BUNDLE="${BUNDLE_NAME}-${VERSION}.tgz"
TOKENS_SRC="${EW_TOKENS_FILE:-edgeworker/edgekv_tokens.js}"

mkdir -p "$OUT_DIR"
WORK=$(mktemp -d)
trap "rm -rf $WORK" EXIT

cp edgeworker/main.js "$WORK/main.js"
cp edgeworker/edgekv.js "$WORK/edgekv.js"
cp "$TOKENS_SRC" "$WORK/edgekv_tokens.js"
sed "s|__VERSION__|${VERSION}|" edgeworker/bundle.json > "$WORK/bundle.json"

# Optional NAMESPACE override (legacy single-namespace bundles only —
# main.js now selects namespace per-host via NAMESPACE_BY_HOST + DEFAULT_NAMESPACE).
if [ -n "$NAMESPACE_OVERRIDE" ]; then
  sed -i.bak "s|^const DEFAULT_NAMESPACE = '[^']*';|const DEFAULT_NAMESPACE = '${NAMESPACE_OVERRIDE}';|" "$WORK/main.js"
  echo "EW build: DEFAULT_NAMESPACE → ${NAMESPACE_OVERRIDE}"
fi
if [ -n "$GROUP_OVERRIDE" ]; then
  sed -i.bak "s|^const GROUP = '[^']*';|const GROUP = '${GROUP_OVERRIDE}';|" "$WORK/main.js"
  echo "EW build: GROUP → ${GROUP_OVERRIDE}"
fi
rm -f "$WORK/main.js.bak"

# Akamai CLI bug workaround: `akamai edgekv create token --save_path` writes
# the token file with a bare namespace key (e.g. `"vanity-manager"`), but the
# bundled edgekv.js helper looks up `"namespace-" + namespace` (see
# edgekv.js:129 in the akamai/edgeworkers-examples helper). Without this
# rewrite, the EW always throws `MISSING ACCESS TOKEN`. Idempotent: skips
# keys already prefixed with `namespace-`.
python3 - "$WORK/edgekv_tokens.js" <<'PY'
import re, sys
p = sys.argv[1]
src = open(p).read()
src2 = re.sub(r'"(?!namespace-)([a-z0-9][a-z0-9_-]*)"\s*:\s*\{', r'"namespace-\1" : {', src)
if src != src2:
    open(p, "w").write(src2)
    print("patched edgekv_tokens.js: prefixed namespace keys with `namespace-`")
PY

(cd "$WORK" && tar -czf "-" main.js edgekv.js edgekv_tokens.js bundle.json) > "$OUT_DIR/$BUNDLE"

echo "built $OUT_DIR/$BUNDLE"
echo "  bundled DEFAULT_NAMESPACE = $(grep -E '^const DEFAULT_NAMESPACE =' "$WORK/main.js")"
echo "  bundled GROUP             = $(grep -E '^const GROUP =' "$WORK/main.js")"
nb=$(grep -c '^  .*:.* .vanity-manager' "$WORK/main.js" 2>/dev/null || echo 0)
echo "  per-host namespace overrides: ${nb}"
