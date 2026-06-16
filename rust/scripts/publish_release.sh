#!/bin/bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: rust/scripts/publish_release.sh <version-path> <release-cid-or-nhash> [tree-name]

Publishes a release directory CID into a mutable release tree and repoints the
"latest" entry to the same CID.

Examples:
  rust/scripts/publish_release.sh v0.2.3 nhash1...
  rust/scripts/publish_release.sh releases/v0.2.3 nhash1... releases/hashtree
EOF
}

if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
    usage
    exit 0
fi

if [ $# -lt 2 ] || [ $# -gt 3 ]; then
    usage >&2
    exit 1
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/release_common.sh"
REPO_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"

version_path="$1"
release_cid="$2"
repo_name="$(infer_repo_name "$REPO_DIR")"
tree_name="${3:-releases/${repo_name}}"
npub="$(current_npub)"

if [[ "$version_path" == */* ]]; then
    latest_path="${version_path%/*}/latest"
else
    latest_path="latest"
fi

htree release publish "$tree_name" "$version_path" "$release_cid"

if [ -z "$npub" ]; then
    echo "Warning: release published, but current npub could not be determined for printed URLs." >&2
    exit 0
fi

cat <<EOF

Canonical:
  htree://${npub}/${tree_name}/${version_path}
  htree://${npub}/${tree_name}/${latest_path}

Direct:
  $(gateway_release_base_url "$npub" "$tree_name" "$version_path")/
  $(gateway_release_base_url "$npub" "$tree_name" "$latest_path")/

Browser:
  https://drive.iris.to/#/${npub}/${tree_name}/${version_path}
  https://drive.iris.to/#/${npub}/${tree_name}/${latest_path}
EOF
