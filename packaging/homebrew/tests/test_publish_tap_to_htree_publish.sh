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
PUBLISHED="${TMP_DIR}/published.git"

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

export LOG_FILE PUBLISHED
cat >"${TMP_DIR}/bin/htree" <<'EOF'
#!/bin/bash
set -euo pipefail
echo "htree:$*" >>"${LOG_FILE}"
case "${1:-}" in
    add)
        test "${3:-}" = --publish
        test "${4:-}" = homebrew-htree-test.git
        git --git-dir="$2" fsck --full --strict >/dev/null
        rm -rf "$PUBLISHED"
        cp -R "$2" "$PUBLISHED"
        printf '  url: nhash1tap\n'
        ;;
    get)
        test "${2:-}" = htree://npub1test/homebrew-htree-test.git
        test "${3:-}" = --output
        test "${HTREE_TEST_FAIL_GET:-0}" = 0 || exit 75
        test -d "$PUBLISHED" || exit 1
        if [ "${HTREE_TEST_REF_CHANGE:-0}" = 1 ]; then
            if [ -e "${PUBLISHED}.read-once" ]; then
                git --git-dir="$PUBLISHED" update-ref refs/heads/concurrent master
            else
                touch "${PUBLISHED}.read-once"
            fi
        fi
        cp -R "$PUBLISHED" "$4"
        ;;
    user) printf 'npub1test\n' ;;
    *) echo "unexpected htree command: $*" >&2; exit 1 ;;
esac
EOF
chmod +x "${TMP_DIR}/bin/htree"
cat >"${TMP_DIR}/bin/curl" <<EOF
#!/bin/bash
set -euo pipefail
echo "curl:\$*" >>"${LOG_FILE}"
EOF
chmod +x "${TMP_DIR}/bin/curl"

# Provision once with the existing creator; updates never infer absence.
"${HOMEBREW_DIR}/create_tap.sh" --version v0.0.0 \
    --release-base-url https://example.invalid/bootstrap/assets \
    --assets-dir "$ASSETS_DIR" --output-dir "${TMP_DIR}/bootstrap.git" >/dev/null
PATH="${TMP_DIR}/bin:$PATH" htree add "${TMP_DIR}/bootstrap.git" \
    --publish homebrew-htree-test.git >/dev/null

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

first="$(git --git-dir="$PUBLISHED" rev-parse master)"
git --git-dir="$PUBLISHED" update-ref refs/tags/prior-release "$first"
retained="$(echo retained | git --git-dir="$PUBLISHED" -c user.name=Test -c user.email=test@example.invalid commit-tree "$first^{tree}")"
git --git-dir="$PUBLISHED" update-ref refs/heads/retained "$retained"
git --git-dir="$PUBLISHED" update-ref refs/remotes/archive/retained "$retained"
git --git-dir="$PUBLISHED" config remote.archive.url "${TMP_DIR}/private-archive.git"
git clone "$PUBLISHED" "${TMP_DIR}/installed" >/dev/null
PATH="${TMP_DIR}/bin:$PATH" "$PUBLISH_TAP_SCRIPT" \
    --version v0.0.2 --assets-dir "$ASSETS_DIR" \
    --release-base-url https://example.invalid/releases/v0.0.2/assets \
    --tap-repo homebrew-htree-test --npub npub1test >"$STDOUT_FILE"
git -C "${TMP_DIR}/installed" fetch origin >/dev/null
git -C "${TMP_DIR}/installed" merge --ff-only origin/master >/dev/null
git --git-dir="$PUBLISHED" merge-base --is-ancestor "$first" master
test "$(git --git-dir="$PUBLISHED" rev-list --count master)" = 3
test "$(git --git-dir="$PUBLISHED" rev-parse refs/tags/prior-release)" = "$first"
test "$(git --git-dir="$PUBLISHED" rev-parse refs/heads/retained)" = "$retained"
test "$(git --git-dir="$PUBLISHED" rev-parse refs/remotes/archive/retained)" = "$retained"
test -z "$(git --git-dir="$PUBLISHED" remote)"
git --git-dir="$PUBLISHED" fsck --full --strict >/dev/null
grep -F 'version "0.0.2"' "${TMP_DIR}/installed/Formula/htree.rb" >/dev/null
second="$(git --git-dir="$PUBLISHED" rev-parse master)"
adds_before="$(grep -c '^htree:add ' "$LOG_FILE")"
PATH="${TMP_DIR}/bin:$PATH" "$PUBLISH_TAP_SCRIPT" \
    --version v0.0.2 --assets-dir "$ASSETS_DIR" \
    --release-base-url https://example.invalid/releases/v0.0.2/assets \
    --tap-repo homebrew-htree-test --npub npub1test >/dev/null
test "$(grep -c '^htree:add ' "$LOG_FILE")" = "$adds_before"
test "$(git --git-dir="$PUBLISHED" rev-parse refs/heads/retained)" = "$retained"
test "$(git --git-dir="$PUBLISHED" rev-parse refs/remotes/archive/retained)" = "$retained"
test -z "$(git --git-dir="$PUBLISHED" remote)"
if PATH="${TMP_DIR}/bin:$PATH" HTREE_TEST_FAIL_GET=1 "$PUBLISH_TAP_SCRIPT" \
    --version v0.0.3 --assets-dir "$ASSETS_DIR" \
    --release-base-url https://example.invalid/releases/v0.0.3/assets \
    --tap-repo homebrew-htree-test --npub npub1test >/dev/null 2>&1; then
    echo "A failed history read must not publish a replacement" >&2
    exit 1
fi
test "$(grep -c '^htree:add ' "$LOG_FILE")" = "$adds_before"
test "$(git --git-dir="$PUBLISHED" rev-parse master)" = "$second"
if PATH="${TMP_DIR}/bin:$PATH" HTREE_TEST_REF_CHANGE=1 "$PUBLISH_TAP_SCRIPT" \
    --version v0.0.3 --assets-dir "$ASSETS_DIR" \
    --release-base-url https://example.invalid/releases/v0.0.3/assets \
    --tap-repo homebrew-htree-test --npub npub1test >/dev/null 2>&1; then
    echo "Concurrent ref changes must stop publication" >&2
    exit 1
fi
test "$(grep -c '^htree:add ' "$LOG_FILE")" = "$adds_before"
test "$(git --git-dir="$PUBLISHED" rev-parse master)" = "$second"
if PATH="${TMP_DIR}/bin:$PATH" "$PUBLISH_TAP_SCRIPT" \
    --version v0.0.3 --initial --assets-dir "$ASSETS_DIR" \
    --release-base-url https://example.invalid/assets \
    --tap-repo homebrew-htree-test --npub npub1test >/dev/null 2>&1; then
    echo "Updater must not expose a history-reset shortcut" >&2
    exit 1
fi
test "$(grep -c '^htree:add ' "$LOG_FILE")" = "$adds_before"
echo "test_publish_tap_to_htree_publish.sh passed"
