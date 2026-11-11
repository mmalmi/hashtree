#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUST_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

TMPDIR="$(mktemp -d)"
cleanup() {
    rm -rf "$TMPDIR"
}
trap cleanup EXIT

BIN_DIR="${TMPDIR}/bin"
REPO_ROOT="${TMPDIR}/hashtree-release-worktree"
mkdir -p "${BIN_DIR}" "${REPO_ROOT}/rust/scripts"

cp "${RUST_DIR}/scripts/publish_release.sh" "${REPO_ROOT}/rust/scripts/publish_release.sh"
cp "${RUST_DIR}/scripts/release_common.sh" "${REPO_ROOT}/rust/scripts/release_common.sh"
chmod +x "${REPO_ROOT}/rust/scripts/publish_release.sh"
git init "${REPO_ROOT}" >/dev/null
git -C "${REPO_ROOT}" remote add origin htree://self/hashtree

cat >"${BIN_DIR}/htree" <<'EOF'
#!/bin/bash
set -euo pipefail

case "${1:-}" in
    user)
        echo "2026-03-31T10:00:00Z INFO loading profile"
        echo "npub1qqqqqqqqqqqqqqqqqqqqq (Release Owner)"
        ;;
    release)
        echo "$*" >>"${TEST_LOG_DIR}/htree.log"
        ;;
    *)
        echo "unexpected htree command: $*" >&2
        exit 1
        ;;
esac
EOF
chmod +x "${BIN_DIR}/htree"

OUTPUT="$(
    PATH="${BIN_DIR}:$PATH" TEST_LOG_DIR="$TMPDIR" \
        "${REPO_ROOT}/rust/scripts/publish_release.sh" v0.2.3 nhash1example
)"

grep -F "release publish releases/hashtree v0.2.3 nhash1example --legacy-keys" "${TMPDIR}/htree.log" >/dev/null
printf '%s\n' "$OUTPUT" | grep -F "htree://npub1qqqqqqqqqqqqqqqqqqqqq/releases/hashtree/v0.2.3" >/dev/null
printf '%s\n' "$OUTPUT" | grep -F "https://upload.iris.to/npub1qqqqqqqqqqqqqqqqqqqqq/releases%2Fhashtree/v0.2.3/" >/dev/null
printf '%s\n' "$OUTPUT" | grep -F "https://upload.iris.to/npub1qqqqqqqqqqqqqqqqqqqqq/releases%2Fhashtree/latest/" >/dev/null
if printf '%s\n' "$OUTPUT" | grep -F "Release Owner" >/dev/null; then
    echo "publish_release output should not include display names from htree user" >&2
    exit 1
fi

echo "test_publish_release_urls.sh passed"
