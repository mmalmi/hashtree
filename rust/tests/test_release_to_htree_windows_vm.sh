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
    "${TMPDIR}/bin" \
    "${TMPDIR}/logs" \
    "${TMPDIR}/out" \
    "${TMPDIR}/release-stage"

cp /Users/sirius/src/hashtree/rust/scripts/release_to_htree.sh "${REPO_ROOT}/rust/scripts/release_to_htree.sh"
cp /Users/sirius/src/hashtree/rust/scripts/release_common.sh "${REPO_ROOT}/rust/scripts/release_common.sh"
cp /Users/sirius/src/hashtree/rust/scripts/write_release_bootstrap_installer.sh "${REPO_ROOT}/rust/scripts/write_release_bootstrap_installer.sh"
cp /Users/sirius/src/hashtree/scripts/stage_repo_release.mjs "${REPO_ROOT}/scripts/stage_repo_release.mjs"
chmod +x "${REPO_ROOT}/rust/scripts/release_to_htree.sh"
chmod +x "${REPO_ROOT}/rust/scripts/write_release_bootstrap_installer.sh"

git init "${REPO_ROOT}" >/dev/null
git -C "${REPO_ROOT}" remote add origin htree://self/hashtree

cat >"${REPO_ROOT}/rust/scripts/build_release_artifacts.sh" <<'EOF'
#!/bin/bash
set -euo pipefail

original_args=("$@")
output_dir=""
windows_artifacts_dir=""
while [ $# -gt 0 ]; do
    case "$1" in
        --output-dir)
            output_dir="$2"
            shift 2
            ;;
        --windows-artifacts-dir)
            windows_artifacts_dir="$2"
            shift 2
            ;;
        *)
            shift
            ;;
    esac
done

mkdir -p "$output_dir"
printf 'build:%s\n' "${original_args[*]}" >>"${TEST_LOG_DIR}/calls.log"

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
printf 'deadbeef  hashtree-aarch64-apple-darwin.tar.gz\n' >"${output_dir}/hashtree-aarch64-apple-darwin.sha256"
rm -rf "$stage_dir"

if [ -n "$windows_artifacts_dir" ]; then
    for binary in htree.exe htree-cashu.exe git-remote-htree.exe; do
        test -f "${windows_artifacts_dir}/${binary}"
    done
    stage_dir="$(mktemp -d)"
    package_dir="${stage_dir}/hashtree"
    mkdir -p "${package_dir}"
    cp "${windows_artifacts_dir}/"*.exe "${package_dir}/"
    printf 'windows readme\n' >"${package_dir}/README.txt"
    python3 - <<'PY' "${stage_dir}" "${output_dir}"
import pathlib
import sys
import zipfile

stage_dir = pathlib.Path(sys.argv[1])
output_dir = pathlib.Path(sys.argv[2])
with zipfile.ZipFile(output_dir / "hashtree-x86_64-pc-windows-msvc.zip", "w", compression=zipfile.ZIP_DEFLATED) as zf:
    for path in sorted((stage_dir / "hashtree").rglob("*")):
        if path.is_file():
            zf.write(path, path.relative_to(stage_dir).as_posix())
PY
    printf 'beadfeed  hashtree-x86_64-pc-windows-msvc.zip\n' >"${output_dir}/hashtree-x86_64-pc-windows-msvc.sha256"
    rm -rf "$stage_dir"
fi
EOF
chmod +x "${REPO_ROOT}/rust/scripts/build_release_artifacts.sh"

cat >"${REPO_ROOT}/rust/scripts/build_windows_vm_artifacts.mjs" <<'EOF'
#!/usr/bin/env node

import { mkdirSync, writeFileSync } from 'node:fs'

const args = process.argv.slice(2)
let outputDir = ''

for (let index = 0; index < args.length; index += 1) {
  if (args[index] === '--output-dir') {
    outputDir = args[index + 1] ?? ''
    index += 1
  }
}

if (!outputDir) {
  throw new Error('missing --output-dir')
}

mkdirSync(outputDir, { recursive: true })
for (const name of ['htree.exe', 'htree-cashu.exe', 'git-remote-htree.exe']) {
  writeFileSync(`${outputDir}/${name}`, `${name}\n`)
}
process.stdout.write(`windows_helper:${outputDir}\n`)
EOF
chmod +x "${REPO_ROOT}/rust/scripts/build_windows_vm_artifacts.mjs"

cat >"${REPO_ROOT}/rust/scripts/publish_release.sh" <<'EOF'
#!/bin/bash
set -euo pipefail
echo "publish_release:$*" >>"${TEST_LOG_DIR}/calls.log"
EOF
chmod +x "${REPO_ROOT}/rust/scripts/publish_release.sh"

cat >"${TMPDIR}/bin/htree" <<'EOF'
#!/bin/bash
set -euo pipefail
case "${1:-}" in
    add)
        echo "htree_add:$2" >>"${TEST_LOG_DIR}/calls.log"
        printf '  url: nhash1release\n'
        ;;
    user)
        printf 'npub1qqqqqqqqqqqqqqqqqqqqq\n'
        ;;
    *)
        echo "unexpected htree command: $*" >&2
        exit 1
        ;;
esac
EOF
chmod +x "${TMPDIR}/bin/htree"

STDOUT_FILE="${TMPDIR}/release_to_htree_windows.stdout"
STDERR_FILE="${TMPDIR}/release_to_htree_windows.stderr"
PATH="${TMPDIR}/bin:$PATH" TEST_LOG_DIR="${TMPDIR}/logs" \
    "${REPO_ROOT}/rust/scripts/release_to_htree.sh" \
    --version v0.2.3 \
    --output-dir "${TMPDIR}/out" \
    --release-stage-dir "${TMPDIR}/release-stage" \
    --skip-homebrew-tap \
    --skip-post-publish-install-checks >"${STDOUT_FILE}" 2>"${STDERR_FILE}"

grep -F "build:--version v0.2.3 --output-dir ${TMPDIR}/out --windows-artifacts-dir" "${TMPDIR}/logs/calls.log" >/dev/null
grep -F "windows_helper:" "${STDOUT_FILE}" >/dev/null
grep -F "Warning: skipping release installer generation because the full macOS/Linux archive set is not present." "${STDERR_FILE}" >/dev/null
test ! -f "${TMPDIR}/out/install.sh"
test -f "${TMPDIR}/out/hashtree-x86_64-pc-windows-msvc.zip"
test ! -f "${TMPDIR}/release-stage/install.sh"
test -f "${TMPDIR}/release-stage/assets/hashtree-x86_64-pc-windows-msvc.zip"
grep -F 'Windows x64 CLI' "${TMPDIR}/release-stage/notes.md" >/dev/null
grep -F "publish_release:v0.2.3 nhash1release releases/hashtree" "${TMPDIR}/logs/calls.log" >/dev/null

echo "test_release_to_htree_windows_vm.sh passed"
