#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUST_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
BUILD_SCRIPT="${RUST_DIR}/scripts/build_release_artifacts.sh"

TMPDIR="$(mktemp -d)"
cleanup() {
    rm -rf "$TMPDIR"
}
trap cleanup EXIT

BIN_DIR="${TMPDIR}/bin"
SOURCE_REPO_DIR="${TMPDIR}/source-repo"
TARGET_DIR="${TMPDIR}/custom-target"
OUTPUT_DIR="${TMPDIR}/out"
LOG_DIR="${TMPDIR}/logs"

mkdir -p "$BIN_DIR" "$LOG_DIR" "${SOURCE_REPO_DIR}/rust"
printf 'lockfile\n' >"${SOURCE_REPO_DIR}/rust/Cargo.lock"

cat >"${BIN_DIR}/rustup" <<'EOF'
#!/bin/bash
set -euo pipefail
printf '%s\n' "$*" >>"${TEST_LOG_DIR}/rustup.log"
EOF
chmod +x "${BIN_DIR}/rustup"

cat >"${BIN_DIR}/uname" <<'EOF'
#!/bin/bash
set -euo pipefail

case "${1:-}" in
    -s) printf 'Darwin\n' ;;
    -m) printf 'arm64\n' ;;
    *) printf 'Darwin\n' ;;
esac
EOF
chmod +x "${BIN_DIR}/uname"

cat >"${BIN_DIR}/cargo" <<'EOF'
#!/bin/bash
set -euo pipefail

printf 'env:%s\npkg-config-allow-cross:%s\nargs:%s\n' "${CARGO_TARGET_DIR:-}" "${PKG_CONFIG_ALLOW_CROSS:-}" "$*" >>"${TEST_LOG_DIR}/cargo.log"
printf 'pwd:%s\n' "$PWD" >>"${TEST_LOG_DIR}/cargo.log"

target=""
args=("$@")
for ((i = 0; i < ${#args[@]}; i++)); do
    if [ "${args[$i]}" = "--target" ]; then
        target="${args[$((i + 1))]}"
        break
    fi
done

if [ -z "$target" ]; then
    echo "missing --target" >&2
    exit 1
fi

release_dir="${CARGO_TARGET_DIR}/${target}/release"
mkdir -p "$release_dir"
for binary in git-remote-htree htree-cashu htree; do
    printf '%s\n' "#!/bin/sh" "echo ${binary}" >"${release_dir}/${binary}"
    chmod +x "${release_dir}/${binary}"
done
EOF
chmod +x "${BIN_DIR}/cargo"

cat >"${BIN_DIR}/cross" <<'EOF'
#!/bin/bash
set -euo pipefail

printf 'env:%s\nargs:%s\n' "${CARGO_TARGET_DIR:-}" "$*" >>"${TEST_LOG_DIR}/cross.log"
printf 'pwd:%s\n' "$PWD" >>"${TEST_LOG_DIR}/cross.log"

target=""
args=("$@")
for ((i = 0; i < ${#args[@]}; i++)); do
    if [ "${args[$i]}" = "--target" ]; then
        target="${args[$((i + 1))]}"
        break
    fi
done

if [ -z "$target" ]; then
    echo "missing --target" >&2
    exit 1
fi

release_dir="${CARGO_TARGET_DIR}/${target}/release"
mkdir -p "$release_dir"
for binary in git-remote-htree htree-cashu htree; do
    printf '%s\n' "#!/bin/sh" "echo ${binary}" >"${release_dir}/${binary}"
    chmod +x "${release_dir}/${binary}"
done
EOF
chmod +x "${BIN_DIR}/cross"

PATH="${BIN_DIR}:$PATH" TEST_LOG_DIR="${LOG_DIR}" "${BUILD_SCRIPT}" \
    --version v0.2.3 \
    --repo-dir "${SOURCE_REPO_DIR}" \
    --output-dir "${OUTPUT_DIR}" \
    --target-dir "${TARGET_DIR}" \
    --targets "aarch64-apple-darwin,x86_64-apple-darwin,x86_64-unknown-linux-musl" \
    --linux-builder cross \
    --cargo-bin cargo \
    --cross-bin cross

grep -Fx "target add aarch64-apple-darwin" "${LOG_DIR}/rustup.log" >/dev/null
grep -Fx "target add x86_64-apple-darwin" "${LOG_DIR}/rustup.log" >/dev/null
grep -F "env:${TARGET_DIR}" "${LOG_DIR}/cargo.log" >/dev/null
grep -F "pwd:${SOURCE_REPO_DIR}/rust" "${LOG_DIR}/cargo.log" >/dev/null
grep -F "args:build --release --target aarch64-apple-darwin -p git-remote-htree -p hashtree-cashu-cli -p hashtree-cli --features hashtree-cli/fips-webrtc --locked" "${LOG_DIR}/cargo.log" >/dev/null
grep -F "args:build --release --target x86_64-apple-darwin -p git-remote-htree -p hashtree-cashu-cli -p hashtree-cli --features hashtree-cli/fips-webrtc --locked" "${LOG_DIR}/cargo.log" >/dev/null
grep -F "pkg-config-allow-cross:1" "${LOG_DIR}/cargo.log" >/dev/null
grep -F "env:${TARGET_DIR}" "${LOG_DIR}/cross.log" >/dev/null
grep -F "pwd:${SOURCE_REPO_DIR}/rust" "${LOG_DIR}/cross.log" >/dev/null
grep -F "args:build --release --target x86_64-unknown-linux-musl -p git-remote-htree -p hashtree-cashu-cli -p hashtree-cli --features hashtree-cli/fips-webrtc --locked" "${LOG_DIR}/cross.log" >/dev/null

test -f "${OUTPUT_DIR}/hashtree-aarch64-apple-darwin.tar.gz"
test -f "${OUTPUT_DIR}/hashtree-x86_64-apple-darwin.tar.gz"
test -f "${OUTPUT_DIR}/hashtree-x86_64-unknown-linux-musl.tar.gz"

echo "test_build_release_invocation.sh passed"
