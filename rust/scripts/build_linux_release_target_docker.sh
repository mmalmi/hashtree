#!/bin/bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: rust/scripts/build_linux_release_target_docker.sh --target <target> --target-dir <dir> [options]

Build the Linux musl release binaries inside a target-native Alpine container.
This avoids relying on generic cross images for the release toolchain while
keeping optional features such as FUSE out of the default published binaries.

Options:
  --target <target>                Linux musl target triple to build
  --target-dir <dir>               Cargo target directory to write into
  --repo-dir <dir>                 Repository root (default: inferred)
  --fips-dir <dir>                 FIPS repository root (default: sibling ../fips)
  --docker-bin <path>              Docker binary to use (default: docker)
  --docker-rust-image <image>      Rust Alpine image to use
  --cargo-bin <path>               Host cargo binary used only to infer a default Rust image
  -h, --help                       Show this help
EOF
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUST_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
DEFAULT_REPO_DIR="$(cd "${RUST_DIR}/.." && pwd)"

TARGET=""
TARGET_DIR=""
REPO_DIR="${DEFAULT_REPO_DIR}"
FIPS_DIR="${FIPS_DIR:-}"
DOCKER_BIN="${DOCKER_BIN:-docker}"
DOCKER_RUST_IMAGE="${DOCKER_RUST_IMAGE:-}"
CARGO_BIN="${CARGO_BIN:-cargo}"

docker_platform_for_target() {
    case "$1" in
        x86_64-unknown-linux-musl)
            printf '%s\n' linux/amd64
            ;;
        aarch64-unknown-linux-musl)
            printf '%s\n' linux/arm64
            ;;
        *)
            echo "Unsupported Docker Linux target: $1" >&2
            exit 1
            ;;
    esac
}

default_docker_rust_image() {
    local version=""
    if command -v "$CARGO_BIN" >/dev/null 2>&1; then
        version="$("$CARGO_BIN" --version 2>/dev/null | awk 'NR == 1 { print $2 }')"
    fi
    if [ -z "$version" ]; then
        version="1.94.1"
    fi
    printf 'rust:%s-alpine3.22\n' "$version"
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
        --target)
            TARGET="${2:-}"
            shift 2
            ;;
        --target-dir)
            TARGET_DIR="${2:-}"
            shift 2
            ;;
        --repo-dir)
            REPO_DIR="${2:-}"
            shift 2
            ;;
        --fips-dir)
            FIPS_DIR="${2:-}"
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

if [ -z "$TARGET" ] || [ -z "$TARGET_DIR" ]; then
    usage >&2
    exit 1
fi

require_command "$DOCKER_BIN"

mkdir -p "$TARGET_DIR"
TARGET_DIR="$(cd "$TARGET_DIR" && pwd)"
REPO_DIR="$(cd "$REPO_DIR" && pwd)"
if [ -z "$FIPS_DIR" ] && [ -f "${REPO_DIR}/../fips/crates/fips-core/Cargo.toml" ]; then
    FIPS_DIR="$(cd "${REPO_DIR}/../fips" && pwd)"
fi
if [ -z "$FIPS_DIR" ] || [ ! -f "${FIPS_DIR}/crates/fips-core/Cargo.toml" ]; then
    echo "FIPS repo not found. Pass --fips-dir /path/to/fips." >&2
    exit 1
fi
FIPS_DIR="$(cd "$FIPS_DIR" && pwd)"

if [ -z "$DOCKER_RUST_IMAGE" ]; then
    DOCKER_RUST_IMAGE="$(default_docker_rust_image)"
fi

platform="$(docker_platform_for_target "$TARGET")"
docker_mounts=(
    -v "${REPO_DIR}:/work"
    -v "${FIPS_DIR}:/fips:ro"
    -v "${TARGET_DIR}:/target-dir"
)
for sibling in cashu-service cashu_spilman_channels nostr-social-graph; do
    sibling_dir="${REPO_DIR}/../${sibling}"
    if [ -d "$sibling_dir" ]; then
        sibling_dir="$(cd "$sibling_dir" && pwd)"
        docker_mounts+=(-v "${sibling_dir}:/${sibling}:ro")
    fi
done

read -r -d '' build_command <<EOF || true
set -euo pipefail
export PATH="\${CARGO_HOME:-/usr/local/cargo}/bin:\$PATH"
apk add --no-cache build-base clang clang-dev lld musl-dev linux-headers pkgconf cmake perl git >/dev/null
mkdir -p /target-dir/release /target-dir/${TARGET}/release
locked_flag=""
if [ -f /work/rust/Cargo.lock ]; then
    locked_flag="--locked"
fi
cargo build --release --target ${TARGET} --target-dir /target-dir \\
    --jobs "\${CARGO_BUILD_JOBS:-4}" \\
    -p git-remote-htree \\
    -p hashtree-cashu-cli \\
    -p hashtree-cli \\
    \$locked_flag
EOF

"$DOCKER_BIN" run --rm --platform "$platform" \
    "${docker_mounts[@]}" \
    -w /work/rust \
    "$DOCKER_RUST_IMAGE" \
    sh -lc "$build_command"
