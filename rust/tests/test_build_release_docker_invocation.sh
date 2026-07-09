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
TARGET_DIR="${TMPDIR}/target"
OUTPUT_DIR="${TMPDIR}/out"
LOG_DIR="${TMPDIR}/logs"
mkdir -p \
    "$BIN_DIR" \
    "$LOG_DIR" \
    "${SOURCE_REPO_DIR}/rust" \
    "${TARGET_DIR}"
printf 'lockfile\n' >"${SOURCE_REPO_DIR}/rust/Cargo.lock"

cat >"${BIN_DIR}/docker" <<'EOF'
#!/bin/bash
set -euo pipefail

printf 'args:%s\n' "$*" >>"${TEST_LOG_DIR}/docker.log"

target_dir=""
target=""
for arg in "$@"; do
    case "$arg" in
        *:/target-dir)
            target_dir="${arg%:/target-dir}"
            ;;
    esac

    if [[ "$arg" == *"--target "* ]]; then
        target="$(printf '%s\n' "$arg" | sed -n 's/.*--target \([^ ]*\).*/\1/p')"
    fi
done

if [ -z "$target_dir" ] || [ -z "$target" ]; then
    echo "missing target-dir mount or --target in docker command" >&2
    exit 1
fi

release_dir="${target_dir}/${target}/release"
mkdir -p "$release_dir"
for binary in git-remote-htree htree-cashu htree; do
    printf '%s\n' "#!/bin/sh" "echo ${binary}" >"${release_dir}/${binary}"
    chmod +x "${release_dir}/${binary}"
done
EOF
chmod +x "${BIN_DIR}/docker"

PATH="${BIN_DIR}:$PATH" TEST_LOG_DIR="${LOG_DIR}" "${BUILD_SCRIPT}" \
    --version v0.2.3 \
    --repo-dir "${SOURCE_REPO_DIR}" \
    --output-dir "${OUTPUT_DIR}" \
    --target-dir "${TARGET_DIR}" \
    --targets "x86_64-unknown-linux-musl" \
    --linux-builder docker \
    --docker-bin docker \
    --docker-rust-image rust:test

grep -F -- "--platform linux/amd64" "${LOG_DIR}/docker.log" >/dev/null
grep -F -- "-v ${SOURCE_REPO_DIR}:/work" "${LOG_DIR}/docker.log" >/dev/null
grep -F -- "-v ${TARGET_DIR}:/target-dir" "${LOG_DIR}/docker.log" >/dev/null
! grep -F -- ":/fips" "${LOG_DIR}/docker.log" >/dev/null
! grep -F -- ":/cashu-service" "${LOG_DIR}/docker.log" >/dev/null
! grep -F -- ":/nostr-social-graph" "${LOG_DIR}/docker.log" >/dev/null
grep -F -- "rust:test" "${LOG_DIR}/docker.log" >/dev/null
grep -F -- "--target x86_64-unknown-linux-musl" "${LOG_DIR}/docker.log" >/dev/null
grep -F -- "--jobs \"\${CARGO_BUILD_JOBS:-4}\"" "${LOG_DIR}/docker.log" >/dev/null
grep -F -- "--locked" "${LOG_DIR}/docker.log" >/dev/null
grep -F -- "apk add --no-cache build-base" "${LOG_DIR}/docker.log" >/dev/null
grep -F -- "mkdir -p /target-dir/release /target-dir/x86_64-unknown-linux-musl/release" "${LOG_DIR}/docker.log" >/dev/null

test -f "${OUTPUT_DIR}/hashtree-x86_64-unknown-linux-musl.tar.gz"
test ! -e "${OUTPUT_DIR}/hashtree-x86_64-unknown-linux-musl.sha256"

echo "test_build_release_docker_invocation.sh passed"
