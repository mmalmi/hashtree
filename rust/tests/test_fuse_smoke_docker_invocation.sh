#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUST_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
RUN_SCRIPT="${RUST_DIR}/scripts/run_fuse_smoke_in_docker.sh"

TMPDIR="$(mktemp -d)"
cleanup() {
    rm -rf "$TMPDIR"
}
trap cleanup EXIT

BIN_DIR="${TMPDIR}/bin"
SOURCE_REPO_DIR="${TMPDIR}/source-repo"
CARGO_HOME_DIR="${TMPDIR}/cargo-home"
LOG_DIR="${TMPDIR}/logs"

mkdir -p "$BIN_DIR" "$LOG_DIR" "${SOURCE_REPO_DIR}/rust" "$CARGO_HOME_DIR"

cat >"${BIN_DIR}/docker" <<'EOF'
#!/bin/bash
set -euo pipefail
printf 'args:%s\n' "$*" >>"${TEST_LOG_DIR}/docker.log"
EOF
chmod +x "${BIN_DIR}/docker"

PATH="${BIN_DIR}:$PATH" TEST_LOG_DIR="${LOG_DIR}" CARGO_HOME="${CARGO_HOME_DIR}" "${RUN_SCRIPT}" \
    --repo-dir "${SOURCE_REPO_DIR}" \
    --docker-bin docker \
    --docker-rust-image rust:test

grep -F -- "--device /dev/fuse" "${LOG_DIR}/docker.log" >/dev/null
grep -F -- "--cap-add SYS_ADMIN" "${LOG_DIR}/docker.log" >/dev/null
grep -F -- "--security-opt apparmor:unconfined" "${LOG_DIR}/docker.log" >/dev/null
grep -F -- "-e CARGO_HOME=/cargo-home" "${LOG_DIR}/docker.log" >/dev/null
grep -F -- "-v ${SOURCE_REPO_DIR}:/work" "${LOG_DIR}/docker.log" >/dev/null
grep -F -- "-v ${CARGO_HOME_DIR}:/cargo-home" "${LOG_DIR}/docker.log" >/dev/null
grep -F -- "-w /work/rust" "${LOG_DIR}/docker.log" >/dev/null
grep -F -- "rust:test" "${LOG_DIR}/docker.log" >/dev/null
grep -F -- "bash -lc" "${LOG_DIR}/docker.log" >/dev/null
grep -F -- "apt-get install -y --no-install-recommends fuse3 pkg-config libfuse3-dev libdbus-1-dev libclang-dev" "${LOG_DIR}/docker.log" >/dev/null
grep -F -- 'RUSTUP_HOME=/usr/local/rustup PATH=/usr/local/cargo/bin:$PATH' "${LOG_DIR}/docker.log" >/dev/null
grep -F -- "cd /work/rust" "${LOG_DIR}/docker.log" >/dev/null
grep -F -- "cargo test -p hashtree-cli --features fuse --test fuse_mount_smoke -- --nocapture" "${LOG_DIR}/docker.log" >/dev/null

echo "test_fuse_smoke_docker_invocation.sh passed"
