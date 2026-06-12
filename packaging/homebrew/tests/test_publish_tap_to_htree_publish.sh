#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HOMEBREW_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
PUBLISH_TAP_SCRIPT="${HOMEBREW_DIR}/publish_tap.sh"

TMP_DIR="$(mktemp -d)"
cleanup() {
    rm -rf "$TMP_DIR"
}
trap cleanup EXIT

ASSETS_DIR="${TMP_DIR}/assets"
STDOUT_FILE="${TMP_DIR}/publish_tap.out"
LOG_FILE="${TMP_DIR}/htree.log"

require_command() {
    local cmd="$1"
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "Missing required command: $cmd" >&2
        exit 1
    fi
}

require_command git
require_command tar
require_command "$PUBLISH_TAP_SCRIPT"

mkdir -p "$ASSETS_DIR" "${TMP_DIR}/bin"

for target in \
    aarch64-apple-darwin \
    x86_64-apple-darwin \
    aarch64-unknown-linux-musl \
    x86_64-unknown-linux-musl
do
    stage_dir="${TMP_DIR}/stage-${target}"
    mkdir -p "${stage_dir}/hashtree"

    cat > "${stage_dir}/hashtree/htree" <<'EOF'
#!/bin/sh
echo htree-publish-test
EOF
    chmod +x "${stage_dir}/hashtree/htree"

    cat > "${stage_dir}/hashtree/htree-cashu" <<'EOF'
#!/bin/sh
echo htree-cashu-publish-test
EOF
    chmod +x "${stage_dir}/hashtree/htree-cashu"

    cat > "${stage_dir}/hashtree/git-remote-htree" <<'EOF'
#!/bin/sh
echo git-remote-htree-publish-test
EOF
    chmod +x "${stage_dir}/hashtree/git-remote-htree"

    (
        cd "$stage_dir"
        tar -czf "${ASSETS_DIR}/hashtree-${target}.tar.gz" hashtree
    )
done

cat >"${TMP_DIR}/bin/htree" <<EOF
#!/bin/bash
set -euo pipefail
echo "htree:\$*" >>"${LOG_FILE}"
if [ "\${1:-}" = "add" ]; then
    printf '  url: nhash1tap\n'
elif [ "\${1:-}" = "user" ]; then
    printf 'npub1test\n'
else
    echo "unexpected htree command: \$*" >&2
    exit 1
fi
EOF
chmod +x "${TMP_DIR}/bin/htree"
cat >"${TMP_DIR}/bin/curl" <<EOF
#!/bin/bash
set -euo pipefail
echo "curl:\$*" >>"${LOG_FILE}"
EOF
chmod +x "${TMP_DIR}/bin/curl"

output="$(
    PATH="${TMP_DIR}/bin:$PATH" "${PUBLISH_TAP_SCRIPT}" \
        --version v0.0.1 \
        --release-base-url "https://upload.iris.to/npub1test/releases%2Fhashtree/v0.0.1/assets" \
        --assets-dir "$ASSETS_DIR" \
        --tap-repo homebrew-htree-test \
        --npub npub1test
)"
printf '%s\n' "$output" >"$STDOUT_FILE"

grep -F "htree:add " "$LOG_FILE" >/dev/null
grep -F -- "--publish homebrew-htree-test.git" "$LOG_FILE" >/dev/null
grep -F "curl:-fsSL --max-time 30 https://upload.iris.to/api/resolve/npub1test/homebrew-htree-test.git?refresh=1" "$LOG_FILE" >/dev/null
grep -F 'htree://self/homebrew-htree-test.git' "$STDOUT_FILE" >/dev/null
grep -F 'https://upload.iris.to/npub1test/homebrew-htree-test.git' "$STDOUT_FILE" >/dev/null
grep -F 'brew tap <user>/<repo> https://upload.iris.to/npub1test/homebrew-htree-test.git' "$STDOUT_FILE" >/dev/null
grep -F 'brew trust --tap <user>/<repo>' "$STDOUT_FILE" >/dev/null

echo "test_publish_tap_to_htree_publish.sh passed"
