#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SOURCE_REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
README_INSTALL_CMD="$(grep -F 'curl -fsSL https://upload.iris.to/' "${SOURCE_REPO_ROOT}/README.md" | grep 'install.sh' | head -n1)"
README_NPUB="$(printf '%s\n' "${README_INSTALL_CMD}" | grep -oE 'npub1[023456789acdefghjklmnpqrstuvwxyz]+' | head -n1)"

if [ -z "${README_INSTALL_CMD}" ] || [ -z "${README_NPUB}" ]; then
    echo "Failed to extract canonical install command from README.md" >&2
    exit 1
fi

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
    "${TMPDIR}/source-repo" \
    "${TMPDIR}/bin" \
    "${TMPDIR}/logs" \
    "${TMPDIR}/out" \
    "${TMPDIR}/release-stage"

cp "${SOURCE_REPO_ROOT}/rust/scripts/release_to_htree.sh" "${REPO_ROOT}/rust/scripts/release_to_htree.sh"
cp "${SOURCE_REPO_ROOT}/rust/scripts/release_common.sh" "${REPO_ROOT}/rust/scripts/release_common.sh"
cp "${SOURCE_REPO_ROOT}/rust/scripts/write_release_bootstrap_installer.sh" "${REPO_ROOT}/rust/scripts/write_release_bootstrap_installer.sh"
cp "${SOURCE_REPO_ROOT}/scripts/stage_repo_release.mjs" "${REPO_ROOT}/scripts/stage_repo_release.mjs"
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
git -C "${REPO_ROOT}" config user.name "Test User"
git -C "${REPO_ROOT}" config user.email "test@example.com"
git -C "${REPO_ROOT}" remote add origin htree://self/hashtree

SOURCE_REPO="${TMPDIR}/source-repo"
git init "${SOURCE_REPO}" >/dev/null
git -C "${SOURCE_REPO}" config user.name "Source User"
git -C "${SOURCE_REPO}" config user.email "source@example.com"
printf 'tagged source\n' >"${SOURCE_REPO}/README.md"
git -C "${SOURCE_REPO}" add README.md >/dev/null
git -C "${SOURCE_REPO}" commit -m "Tagged source" >/dev/null
SOURCE_COMMIT="$(git -C "${SOURCE_REPO}" rev-parse HEAD)"

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
    printf 'deadbeef  hashtree-%s.tar.gz\n' "$target" >"${output_dir}/hashtree-${target}.sha256"
done

echo "build:$*" >>"${TEST_LOG_DIR}/calls.log"
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
if [ "${FAIL_HOME_TAP:-0}" = "1" ]; then
    exit 1
fi
EOF
chmod +x "${REPO_ROOT}/packaging/homebrew/publish_tap.sh"

cat >"${REPO_ROOT}/rust/scripts/publish.sh" <<'EOF'
#!/bin/bash
set -euo pipefail
echo "cargo_publish:$*" >>"${TEST_LOG_DIR}/calls.log"
EOF
chmod +x "${REPO_ROOT}/rust/scripts/publish.sh"

cat >"${REPO_ROOT}/rust/scripts/test_install_matrix.sh" <<'EOF'
#!/bin/bash
set -euo pipefail
echo "install_matrix:$*" >>"${TEST_LOG_DIR}/calls.log"
if [ "${FAIL_INSTALL_MATRIX:-0}" = "1" ]; then
    exit 1
fi
EOF
chmod +x "${REPO_ROOT}/rust/scripts/test_install_matrix.sh"

cat >"${TMPDIR}/bin/htree" <<EOF
#!/bin/bash
set -euo pipefail
case "\${1:-}" in
    add)
        echo "htree_add:\$2" >>"\${TEST_LOG_DIR}/calls.log"
        printf '  url: nhash1release\n'
        ;;
    user)
        printf '2026-03-31T10:00:00Z INFO loading profile\n'
        printf '${README_NPUB} (Release Owner)\n'
        ;;
    *)
        echo "unexpected htree command: \$*" >&2
        exit 1
        ;;
esac
EOF
chmod +x "${TMPDIR}/bin/htree"

PATH="${TMPDIR}/bin:$PATH" TEST_LOG_DIR="${TMPDIR}/logs" \
    "${REPO_ROOT}/rust/scripts/release_to_htree.sh" \
    --version v0.2.3 \
    --repo-dir "${SOURCE_REPO}" \
    --output-dir "${TMPDIR}/out" \
    --release-stage-dir "${TMPDIR}/release-stage" >/dev/null

grep -F "htree_add:${TMPDIR}/release-stage" "${TMPDIR}/logs/calls.log" >/dev/null
test -f "${TMPDIR}/release-stage/release.json"
test -f "${TMPDIR}/release-stage/notes.md"
test -f "${TMPDIR}/release-stage/install.sh"
test -f "${TMPDIR}/release-stage/assets/hashtree-aarch64-apple-darwin.tar.gz"
grep -F "\"commit\": \"${SOURCE_COMMIT}\"" "${TMPDIR}/release-stage/release.json" >/dev/null
grep -F "## Changelog" "${TMPDIR}/release-stage/notes.md" >/dev/null
grep -F "Added release changelog coverage to the staged repo notes." "${TMPDIR}/release-stage/notes.md" >/dev/null

grep -F "publish_release:v0.2.3 nhash1release releases/hashtree" "${TMPDIR}/logs/calls.log" >/dev/null
grep -F "publish_tap:--version v0.2.3 --release-base-url https://upload.iris.to/${README_NPUB}/releases%2Fhashtree/v0.2.3/assets --assets-dir ${TMPDIR}/out --tap-repo homebrew-hashtree" "${TMPDIR}/logs/calls.log" >/dev/null
grep -F "install_matrix:" "${TMPDIR}/logs/calls.log" >/dev/null
test -f "${TMPDIR}/out/install.sh"
grep -F "BASE_URL=\"https://upload.iris.to/${README_NPUB}/releases%2Fhashtree/v0.2.3\"" "${TMPDIR}/out/install.sh" >/dev/null
grep -F 'ASSET_BASE_URL="${BASE_URL}/assets"' "${TMPDIR}/out/install.sh" >/dev/null
grep -F "hashtree-install: error:" "${TMPDIR}/out/install.sh" >/dev/null
grep -F 'fetch_http=$(curl -fSL -o "$fetch_out" -w '\''%{http_code}'\'' "$fetch_url") || fetch_rc=$?' "${TMPDIR}/out/install.sh" >/dev/null
grep -F 'tar -tzf "$archive_path" >/dev/null 2>&1 || die "downloaded file is not a valid gzip tar archive: $archive_path (download may be corrupt)"' "${TMPDIR}/out/install.sh" >/dev/null
grep -F '[ -d "$packaged_dir" ] || die "expected directory '\''hashtree/'\'' not found in archive (archive layout may have changed)"' "${TMPDIR}/out/install.sh" >/dev/null
grep -F './install.sh "$@"' "${TMPDIR}/out/install.sh" >/dev/null

PORT_FILE="${TMPDIR}/http-port"
SERVER_LOG="${TMPDIR}/http-server.log"
python3 - <<'PY' "${TMPDIR}/release-stage" "${PORT_FILE}" >"${SERVER_LOG}" 2>&1 &
import functools
import http.server
import socketserver
import sys

directory = sys.argv[1]
port_file = sys.argv[2]
handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory=directory)

with socketserver.TCPServer(("127.0.0.1", 0), handler) as httpd:
    with open(port_file, "w", encoding="utf-8") as fh:
        fh.write(str(httpd.server_address[1]))
    httpd.serve_forever()
PY
SERVER_PID=$!
cleanup() {
    if [ -n "${SERVER_PID:-}" ]; then
        kill "${SERVER_PID}" 2>/dev/null || true
        wait "${SERVER_PID}" 2>/dev/null || true
    fi
    rm -rf "$TMPDIR"
}
while [ ! -s "${PORT_FILE}" ]; do
    sleep 0.1
done
PORT="$(cat "${PORT_FILE}")"
perl -0pi -e "s|^BASE_URL=.*$|BASE_URL=\"http://127.0.0.1:${PORT}\"|m" "${TMPDIR}/release-stage/install.sh"
BOOTSTRAP_HOME="${TMPDIR}/bootstrap-home"
BOOTSTRAP_BIN="${TMPDIR}/bootstrap-bin"
mkdir -p "${BOOTSTRAP_HOME}"
env HOME="${BOOTSTRAP_HOME}" PATH="/usr/bin:/bin" /bin/bash "${TMPDIR}/release-stage/install.sh" "${BOOTSTRAP_BIN}"
test -x "${BOOTSTRAP_BIN}/htree"
test -x "${BOOTSTRAP_BIN}/htree-cashu"
test -x "${BOOTSTRAP_BIN}/git-remote-htree"
kill "${SERVER_PID}" 2>/dev/null || true
wait "${SERVER_PID}" 2>/dev/null || true
SERVER_PID=""

README_GATEWAY_ROOT="${TMPDIR}/readme-gateway"
README_RELEASE_ROOTS=(
    "${README_GATEWAY_ROOT}/${README_NPUB}/releases%2Fhashtree"
    "${README_GATEWAY_ROOT}/${README_NPUB}/releases/hashtree"
)
for README_RELEASE_ROOT in "${README_RELEASE_ROOTS[@]}"; do
    mkdir -p "${README_RELEASE_ROOT}/latest" "${README_RELEASE_ROOT}/v0.2.3/assets"
    cp "${TMPDIR}/out/install.sh" "${README_RELEASE_ROOT}/latest/install.sh"
    cp "${TMPDIR}/out"/hashtree-* "${README_RELEASE_ROOT}/v0.2.3/assets/"
done

PORT_FILE="${TMPDIR}/readme-http-port"
SERVER_LOG="${TMPDIR}/readme-http-server.log"
python3 - <<'PY' "${README_GATEWAY_ROOT}" "${PORT_FILE}" >"${SERVER_LOG}" 2>&1 &
import functools
import http.server
import socketserver
import sys

directory = sys.argv[1]
port_file = sys.argv[2]
handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory=directory)

with socketserver.TCPServer(("127.0.0.1", 0), handler) as httpd:
    with open(port_file, "w", encoding="utf-8") as fh:
        fh.write(str(httpd.server_address[1]))
    httpd.serve_forever()
PY
SERVER_PID=$!
while [ ! -s "${PORT_FILE}" ]; do
    sleep 0.1
done
PORT="$(cat "${PORT_FILE}")"
for README_RELEASE_ROOT in "${README_RELEASE_ROOTS[@]}"; do
    perl -0pi -e "s|https://upload\\.iris\\.to|http://127.0.0.1:${PORT}|g" "${README_RELEASE_ROOT}/latest/install.sh"
done
LOCAL_README_INSTALL_CMD="$(printf '%s\n' "${README_INSTALL_CMD}" | sed "s|https://upload.iris.to|http://127.0.0.1:${PORT}|")"
README_HOME="${TMPDIR}/readme-home"
mkdir -p "${README_HOME}"
env HOME="${README_HOME}" PATH="/usr/bin:/bin" /bin/bash -lc "${LOCAL_README_INSTALL_CMD}"
test -x "${README_HOME}/.local/bin/htree"
test -x "${README_HOME}/.local/bin/htree-cashu"
test -x "${README_HOME}/.local/bin/git-remote-htree"
kill "${SERVER_PID}" 2>/dev/null || true
wait "${SERVER_PID}" 2>/dev/null || true
SERVER_PID=""

if grep -F "cargo_publish:" "${TMPDIR}/logs/calls.log" >/dev/null; then
    echo "release_to_htree should not cargo publish unless requested" >&2
    exit 1
fi

rm -f "${TMPDIR}/logs/calls.log"
PATH="${TMPDIR}/bin:$PATH" TEST_LOG_DIR="${TMPDIR}/logs" \
    "${REPO_ROOT}/rust/scripts/release_to_htree.sh" \
    --version v0.2.3 \
    --output-dir "${TMPDIR}/out" \
    --cargo-publish >/dev/null

grep -F "cargo_publish:" "${TMPDIR}/logs/calls.log" >/dev/null
publish_release_line="$(grep -n '^publish_release:' "${TMPDIR}/logs/calls.log" | cut -d: -f1)"
publish_tap_line="$(grep -n '^publish_tap:' "${TMPDIR}/logs/calls.log" | cut -d: -f1)"
install_matrix_line="$(grep -n '^install_matrix:' "${TMPDIR}/logs/calls.log" | cut -d: -f1)"
cargo_publish_line="$(grep -n '^cargo_publish:' "${TMPDIR}/logs/calls.log" | cut -d: -f1)"

if [ -z "$publish_release_line" ] || [ -z "$publish_tap_line" ] || [ -z "$install_matrix_line" ] || [ -z "$cargo_publish_line" ]; then
    echo "Expected publish_release, publish_tap, install_matrix, and cargo_publish calls" >&2
    exit 1
fi

if [ "$publish_tap_line" -le "$publish_release_line" ] || [ "$install_matrix_line" -le "$publish_tap_line" ] || [ "$cargo_publish_line" -le "$install_matrix_line" ]; then
    echo "Expected cargo publish to run after release publication, tap publication, and live install checks" >&2
    exit 1
fi

rm -f "${TMPDIR}/logs/calls.log"
STDOUT_FILE="${TMPDIR}/release_to_htree_homebrew.out"
STDERR_FILE="${TMPDIR}/release_to_htree_homebrew.err"
PATH="${TMPDIR}/bin:$PATH" TEST_LOG_DIR="${TMPDIR}/logs" FAIL_HOME_TAP=1 \
    "${REPO_ROOT}/rust/scripts/release_to_htree.sh" \
    --version v0.2.3 \
    --output-dir "${TMPDIR}/out" >"${STDOUT_FILE}" 2>"${STDERR_FILE}"

grep -F "Warning: Homebrew tap update failed; release artifacts are still published." "${STDERR_FILE}" >/dev/null

rm -f "${TMPDIR}/logs/calls.log"
STDOUT_FILE="${TMPDIR}/release_to_htree_install_checks.out"
STDERR_FILE="${TMPDIR}/release_to_htree_install_checks.err"
PATH="${TMPDIR}/bin:$PATH" TEST_LOG_DIR="${TMPDIR}/logs" FAIL_INSTALL_MATRIX=1 \
    "${REPO_ROOT}/rust/scripts/release_to_htree.sh" \
    --version v0.2.3 \
    --output-dir "${TMPDIR}/out" >"${STDOUT_FILE}" 2>"${STDERR_FILE}"

grep -F "Warning: post-publish install checks reported failures; release artifacts remain published." "${STDERR_FILE}" >/dev/null

echo "test_release_to_htree_homebrew.sh passed"
