#!/usr/bin/env bash
# ew-deploy.sh — upload EW bundle + activate on staging or production.
# Usage: scripts/ew-deploy.sh <ew-name> <version> <STAGING|PRODUCTION>
#
# Requires: the Akamai CLI with edgeworkers package installed, and the
# EdgeWorker ID for the named EW already provisioned in the property.
# The EW_ID is read from provision.env (field EW_ID) or from ~/.akamai-cli/ew-id.
set -euo pipefail
cd "$(dirname "$0")/.."

NAME="${1:-vanity-manager}"
VERSION="${2:-$(git describe --always --dirty 2>/dev/null || echo dev)}"
NETWORK="${3:-STAGING}"
BUNDLE="dist/vanity-manager-${VERSION}.tgz"

test -f "$BUNDLE" || { echo "bundle missing: $BUNDLE — run \`make ew-build\` first"; exit 1; }

# Resolve EW_ID
if [ -f provision.env ] && grep -q '^EW_ID=' provision.env; then
  EW_ID=$(grep '^EW_ID=' provision.env | cut -d= -f2-)
elif [ -f ~/.akamai-cli/ew-id ]; then
  EW_ID=$(cat ~/.akamai-cli/ew-id)
else
  echo "EW_ID not set. Create the EdgeWorker once via the Akamai console/API,"
  echo "then add EW_ID=<id> to provision.env or ~/.akamai-cli/ew-id."
  exit 1
fi

echo "==> uploading $BUNDLE to EW $EW_ID"
akamai edgeworkers upload --bundle "$BUNDLE" "$EW_ID"

# Parse the version the upload registered
UPLOADED_VERSION=$(akamai edgeworkers list-versions "$EW_ID" --jsonout \
  | jq -r '.data.versions | sort_by(.createdTime) | reverse | .[0].version')
echo "==> activating version $UPLOADED_VERSION on $NETWORK"
akamai edgeworkers activate "$EW_ID" "$NETWORK" "$UPLOADED_VERSION"

echo "done — version $UPLOADED_VERSION activating on $NETWORK"
echo "monitor: akamai edgeworkers list-activations $EW_ID"
