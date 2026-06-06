#!/bin/bash
set -euo pipefail

TMPDIR="$(mktemp -d)"
REPO_ROOT="${TMPDIR}/hashtree-release-worktree"
cleanup() {
    rm -rf "$TMPDIR"
}
trap cleanup EXIT

mkdir -p \
    "${REPO_ROOT}/rust/scripts" \
    "${REPO_ROOT}/scripts" \
    "${REPO_ROOT}/packaging/homebrew" \
    "${TMPDIR}/bin" \
    "${TMPDIR}/logs" \
    "${TMPDIR}/out"

cp /Users/sirius/src/hashtree/rust/scripts/release_to_htree.sh "${REPO_ROOT}/rust/scripts/release_to_htree.sh"
cp /Users/sirius/src/hashtree/rust/scripts/release_common.sh "${REPO_ROOT}/rust/scripts/release_common.sh"
cp /Users/sirius/src/hashtree/rust/scripts/write_release_bootstrap_installer.sh "${REPO_ROOT}/rust/scripts/write_release_bootstrap_installer.sh"
cp /Users/sirius/src/hashtree/scripts/stage_repo_release.mjs "${REPO_ROOT}/scripts/stage_repo_release.mjs"
chmod +x "${REPO_ROOT}/rust/scripts/release_to_htree.sh"
chmod +x "${REPO_ROOT}/rust/scripts/write_release_bootstrap_installer.sh"
cat >"${REPO_ROOT}/rust/CHANGELOG.md" <<'EOF'
# Changelog

## 0.2.3 - 2026-04-16

Changes since the previous release.

### Improved

- Added release changelog coverage to the staged repo notes.
EOF

git init "${REPO_ROOT}" >/dev/null
git -C "${REPO_ROOT}" remote add origin htree://self/hashtree

cat >"${REPO_ROOT}/rust/scripts/build_release_artifacts.sh" <<'EOF'
#!/bin/bash
set -euo pipefail

output_dir=""
while [ $# -gt 0 ]; do
    case "$1" in
        --output-dir)
            output_dir="$2"
            shift 2
            ;;
        *)
            shift
            ;;
    esac
done

mkdir -p "$output_dir"
stage_dir="$(mktemp -d)"
package_dir="${stage_dir}/hashtree"
mkdir -p "${package_dir}"

cat >"${package_dir}/install.sh" <<'SCRIPT'
#!/bin/bash
set -euo pipefail
install_dir="${1:-$HOME/.local/bin}"
mkdir -p "$install_dir"
install -m 755 htree htree-cashu git-remote-htree "$install_dir/"
SCRIPT
chmod +x "${package_dir}/install.sh"

for binary in htree htree-cashu git-remote-htree; do
    printf '#!/bin/sh\necho %s\n' "$binary" >"${package_dir}/${binary}"
    chmod +x "${package_dir}/${binary}"
done

(
    cd "$stage_dir"
    tar -czf "${output_dir}/hashtree-aarch64-apple-darwin.tar.gz" hashtree
)
rm -rf "$stage_dir"
EOF
chmod +x "${REPO_ROOT}/rust/scripts/build_release_artifacts.sh"

cat >"${REPO_ROOT}/rust/scripts/publish_release.sh" <<'EOF'
#!/bin/bash
set -euo pipefail
echo "publish_release:$*" >>"${TEST_LOG_DIR}/calls.log"
EOF
chmod +x "${REPO_ROOT}/rust/scripts/publish_release.sh"

cat >"${REPO_ROOT}/packaging/homebrew/publish_tap.sh" <<'EOF'
#!/bin/bash
set -euo pipefail
echo "publish_tap:$*" >>"${TEST_LOG_DIR}/calls.log"
EOF
chmod +x "${REPO_ROOT}/packaging/homebrew/publish_tap.sh"

cat >"${TMPDIR}/bin/htree" <<'EOF'
#!/bin/bash
set -euo pipefail
case "${1:-}" in
    user)
        printf 'npub1qqqqqqqqqqqqqqqqqqqqq\n'
        ;;
    add)
        echo "htree_add:$2" >>"${TEST_LOG_DIR}/calls.log"
        printf '  url: nhash1release\n'
        ;;
    *)
        echo "unexpected htree command: $*" >&2
        exit 1
        ;;
esac
EOF
chmod +x "${TMPDIR}/bin/htree"

STDOUT_FILE="${TMPDIR}/release_to_htree_partial.stdout"
STDERR_FILE="${TMPDIR}/release_to_htree_partial.stderr"
if PATH="${TMPDIR}/bin:$PATH" TEST_LOG_DIR="${TMPDIR}/logs" \
    "${REPO_ROOT}/rust/scripts/release_to_htree.sh" \
    --version v0.2.3 \
    --output-dir "${TMPDIR}/out" >"${STDOUT_FILE}" 2>"${STDERR_FILE}"
then
    echo "release_to_htree.sh should fail when Homebrew archives are incomplete" >&2
    exit 1
fi

grep -F "does not contain the full macOS/Linux archive set required for the Homebrew tap" "${STDERR_FILE}" >/dev/null
if [ -f "${TMPDIR}/logs/calls.log" ]; then
    if grep -E '^(publish_release|publish_tap|htree_add):' "${TMPDIR}/logs/calls.log" >/dev/null; then
        echo "release_to_htree.sh should not publish release data when Homebrew archives are incomplete" >&2
        exit 1
    fi
fi

cat >"${REPO_ROOT}/rust/scripts/build_release_artifacts.sh" <<'EOF'
#!/bin/bash
set -euo pipefail

output_dir=""
while [ $# -gt 0 ]; do
    case "$1" in
        --output-dir)
            output_dir="$2"
            shift 2
            ;;
        *)
            shift
            ;;
    esac
done

mkdir -p "$output_dir"
make_unix_archive() {
    local target="$1"
    local stage_dir package_dir
    stage_dir="$(mktemp -d)"
    package_dir="${stage_dir}/hashtree"
    mkdir -p "${package_dir}"

    cat >"${package_dir}/install.sh" <<'SCRIPT'
#!/bin/bash
set -euo pipefail
install_dir="${1:-$HOME/.local/bin}"
mkdir -p "$install_dir"
install -m 755 htree htree-cashu git-remote-htree "$install_dir/"
SCRIPT
    chmod +x "${package_dir}/install.sh"

    for binary in htree htree-cashu git-remote-htree; do
        printf '#!/bin/sh\necho %s\n' "$binary" >"${package_dir}/${binary}"
        chmod +x "${package_dir}/${binary}"
    done

    (
        cd "$stage_dir"
        tar -czf "${output_dir}/hashtree-${target}.tar.gz" hashtree
    )
    rm -rf "$stage_dir"
}

for target in \
    aarch64-apple-darwin \
    x86_64-apple-darwin \
    aarch64-unknown-linux-musl \
    x86_64-unknown-linux-musl
do
    make_unix_archive "$target"
done
EOF
chmod +x "${REPO_ROOT}/rust/scripts/build_release_artifacts.sh"

rm -rf "${TMPDIR}/out" "${TMPDIR}/logs/calls.log"
STDOUT_FILE="${TMPDIR}/release_to_htree_no_windows.stdout"
STDERR_FILE="${TMPDIR}/release_to_htree_no_windows.stderr"
if PATH="${TMPDIR}/bin:$PATH" TEST_LOG_DIR="${TMPDIR}/logs" \
    "${REPO_ROOT}/rust/scripts/release_to_htree.sh" \
    --version v0.2.3 \
    --output-dir "${TMPDIR}/out" >"${STDOUT_FILE}" 2>"${STDERR_FILE}"
then
    echo "release_to_htree.sh should fail when the Windows archive is missing from a full release" >&2
    exit 1
fi

grep -F "does not contain the Windows x64 archive required for a full-platform release" "${STDERR_FILE}" >/dev/null
if [ -f "${TMPDIR}/logs/calls.log" ]; then
    if grep -E '^(publish_release|publish_tap|htree_add):' "${TMPDIR}/logs/calls.log" >/dev/null; then
        echo "release_to_htree.sh should not publish release data when the Windows archive is missing" >&2
        exit 1
    fi
fi

echo "test_release_to_htree_requires_homebrew_archives.sh passed"
