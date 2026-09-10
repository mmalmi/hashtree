#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HOMEBREW_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
PUBLISH_TAP_SCRIPT="${HOMEBREW_DIR}/publish_tap.sh"

TMP_DIR="$(mktemp -d)"
SERVER_PID=""
cleanup() {
    if [ -n "$SERVER_PID" ]; then
        kill "$SERVER_PID" >/dev/null 2>&1 || true
        wait "$SERVER_PID" >/dev/null 2>&1 || true
    fi
    rm -rf "$TMP_DIR"
}
trap cleanup EXIT

ROOT_DIR="${TMP_DIR}/root"
ASSETS_DIR="${ROOT_DIR}/assets"
DEST_REPO="${TMP_DIR}/dest.git"
CLONE_DIR="${TMP_DIR}/clone"
PORT=18082
STDOUT_FILE="${TMP_DIR}/publish_tap.out"

require_command() {
    local cmd="$1"
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "Missing required command: $cmd" >&2
        exit 1
    fi
}

require_command git
require_command python3
require_command tar
require_command "$PUBLISH_TAP_SCRIPT"

mkdir -p "$ASSETS_DIR"
git init --bare "$DEST_REPO" >/dev/null

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

(
    cd "$ROOT_DIR"
    python3 -m http.server "$PORT" --bind 127.0.0.1 >/dev/null 2>&1
) &
SERVER_PID=$!
sleep 1

output="$("${PUBLISH_TAP_SCRIPT}" \
    --version v0.0.1 \
    --release-base-url "http://127.0.0.1:${PORT}/assets" \
    --assets-dir "$ASSETS_DIR" \
    --push-url "$DEST_REPO" \
    --tap-repo homebrew-htree-test \
    --npub npub1test)"
printf '%s\n' "$output" >"$STDOUT_FILE"

git clone -b master "$DEST_REPO" "$CLONE_DIR" >/dev/null
test -f "${CLONE_DIR}/Formula/htree.rb"
test -L "${CLONE_DIR}/Aliases/hashtree"
grep -F 'http://127.0.0.1:18082/assets/hashtree-aarch64-apple-darwin.tar.gz?v=v0.0.1' "${CLONE_DIR}/Formula/htree.rb" >/dev/null
grep -F 'https://upload.iris.to/npub1test/homebrew-htree-test/.git' "$STDOUT_FILE" >/dev/null
grep -F 'brew install htree' "$STDOUT_FILE" >/dev/null
grep -F 'brew install hashtree' "$STDOUT_FILE" >/dev/null
if grep -F "${DEST_REPO}" "$STDOUT_FILE" >/dev/null; then
    echo "publish_tap output should not expose local push URLs" >&2
    exit 1
fi

first="$(git -C "$CLONE_DIR" rev-parse HEAD)"
git -C "$DEST_REPO" update-ref refs/tags/prior-release "$first"
retained="$(echo retained | git -C "$DEST_REPO" -c user.name=Test -c user.email=test@example.invalid commit-tree "$first^{tree}")"
git -C "$DEST_REPO" update-ref refs/heads/retained "$retained"
printf 'Retain custom tap content\n' >"${CLONE_DIR}/README.md"
git -C "$CLONE_DIR" add README.md
git -C "$CLONE_DIR" -c user.name=Test -c user.email=test@example.invalid commit -m 'Add custom tap content' >/dev/null
git -C "$CLONE_DIR" push origin master >/dev/null
"${PUBLISH_TAP_SCRIPT}" \
    --version v0.0.2 --release-base-url "http://127.0.0.1:${PORT}/assets" \
    --assets-dir "$ASSETS_DIR" --push-url "$DEST_REPO" \
    --tap-repo homebrew-htree-test --npub npub1test >"$STDOUT_FILE"
git -C "$CLONE_DIR" fetch origin >/dev/null
git -C "$CLONE_DIR" merge --ff-only origin/master >/dev/null
git -C "$CLONE_DIR" merge-base --is-ancestor "$first" HEAD
test "$(git -C "$CLONE_DIR" rev-list --count HEAD)" = 3
test "$(git -C "$DEST_REPO" rev-parse refs/tags/prior-release)" = "$first"
test "$(git -C "$DEST_REPO" rev-parse refs/heads/retained)" = "$retained"
git -C "$DEST_REPO" fsck --full --strict >/dev/null
grep -F 'version "0.0.2"' "${CLONE_DIR}/Formula/htree.rb" >/dev/null
grep -Fx "Retain custom tap content" "${CLONE_DIR}/README.md" >/dev/null
second="$(git -C "$DEST_REPO" rev-parse master)"
"${PUBLISH_TAP_SCRIPT}" \
    --version v0.0.2 --release-base-url "http://127.0.0.1:${PORT}/assets" \
    --assets-dir "$ASSETS_DIR" --push-url "$DEST_REPO" \
    --tap-repo homebrew-htree-test --npub npub1test >/dev/null
test "$(git -C "$DEST_REPO" rev-parse master)" = "$second"
if "${PUBLISH_TAP_SCRIPT}" \
    --version v0.0.3 --release-base-url "http://127.0.0.1:${PORT}/assets" \
    --assets-dir "$ASSETS_DIR" --push-url "${TMP_DIR}/missing.git" \
    --tap-repo homebrew-htree-test --npub npub1test >/dev/null 2>&1; then
    echo "Missing/inaccessible destination must fail closed" >&2
    exit 1
fi
test ! -e "${TMP_DIR}/missing.git"
echo "test_publish_tap_to_file_remote.sh passed"
