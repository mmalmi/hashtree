#!/bin/bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: rust/scripts/run_fuse_smoke_in_docker.sh [options]

Run the FUSE mount smoke test inside a privileged Debian-based Rust container.
This avoids depending on the host runner's bare-metal FUSE userspace setup.

Options:
  --repo-dir <dir>                 Repository root (default: inferred)
  --docker-bin <path>              Docker binary to use (default: docker)
  --docker-rust-image <image>      Rust Debian image to use
  --cargo-bin <path>               Host cargo binary used only to infer a default Rust image
  -h, --help                       Show this help
EOF
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUST_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
DEFAULT_REPO_DIR="$(cd "${RUST_DIR}/.." && pwd)"

REPO_DIR="${DEFAULT_REPO_DIR}"
DOCKER_BIN="${DOCKER_BIN:-docker}"
DOCKER_RUST_IMAGE="${DOCKER_RUST_IMAGE:-}"
CARGO_BIN="${CARGO_BIN:-cargo}"
HOST_CARGO_HOME="${CARGO_HOME:-${HOME}/.cargo}"

default_docker_rust_image() {
    local version=""
    if command -v "$CARGO_BIN" >/dev/null 2>&1; then
        version="$("$CARGO_BIN" --version 2>/dev/null | awk 'NR == 1 { print $2 }')"
    fi
    if [ -z "$version" ]; then
        version="1.94.1"
    fi
    printf 'rust:%s-bookworm\n' "$version"
}

require_command() {
    local cmd="$1"
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "Missing required command: $cmd" >&2
        exit 1
    fi
}

while [ $# -gt 0 ]; do
    case "$1" in
        --repo-dir)
            REPO_DIR="${2:-}"
            shift 2
            ;;
        --docker-bin)
            DOCKER_BIN="${2:-}"
            shift 2
            ;;
        --docker-rust-image)
            DOCKER_RUST_IMAGE="${2:-}"
            shift 2
            ;;
        --cargo-bin)
            CARGO_BIN="${2:-}"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "Unknown argument: $1" >&2
            usage >&2
            exit 1
            ;;
    esac
done

require_command "$DOCKER_BIN"

mkdir -p "$HOST_CARGO_HOME"
HOST_CARGO_HOME="$(cd "$HOST_CARGO_HOME" && pwd)"
REPO_DIR="$(cd "$REPO_DIR" && pwd)"

if [ -z "$DOCKER_RUST_IMAGE" ]; then
    DOCKER_RUST_IMAGE="$(default_docker_rust_image)"
fi

read -r -d '' test_command <<'EOF' || true
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update >/dev/null
apt-get install -y --no-install-recommends fuse3 pkg-config libfuse3-dev libdbus-1-dev libclang-dev ca-certificates git >/dev/null
mkdir -p /cargo-home /work/rust/target
if ! getent group "${HOST_GID}" >/dev/null; then
    groupadd -g "${HOST_GID}" codex
fi
group_name="$(getent group "${HOST_GID}" | cut -d: -f1 | head -n1)"
if ! getent passwd "${HOST_UID}" >/dev/null; then
    useradd -m -u "${HOST_UID}" -g "${group_name}" codex
fi
user_name="$(getent passwd "${HOST_UID}" | cut -d: -f1 | head -n1)"
chown "${HOST_UID}:${HOST_GID}" /cargo-home /work/rust/target
su "${user_name}" -s /bin/sh -c 'export CARGO_HOME=/cargo-home RUSTUP_HOME=/usr/local/rustup PATH=/usr/local/cargo/bin:$PATH && cd /work/rust && cargo test -p hashtree-cli --features fuse --test fuse_mount_smoke -- --nocapture'
EOF

"$DOCKER_BIN" run --rm \
    --device /dev/fuse \
    --cap-add SYS_ADMIN \
    --security-opt apparmor:unconfined \
    -e CARGO_HOME=/cargo-home \
    -e RUST_BACKTRACE="${RUST_BACKTRACE:-1}" \
    -e RUST_LOG="${RUST_LOG:-info}" \
    -e HOST_UID="$(id -u)" \
    -e HOST_GID="$(id -g)" \
    -v "${REPO_DIR}:/work" \
    -v "${HOST_CARGO_HOME}:/cargo-home" \
    -w /work/rust \
    "$DOCKER_RUST_IMAGE" \
    bash -lc "$test_command"
