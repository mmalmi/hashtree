#!/bin/bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: rust/scripts/run_lmdb_cross_libc_test_in_docker.sh [options]

Check that two processes linked against different C libraries (glibc, musl)
cannot hang each other on one LMDB lock file. Builds the vendored LMDB with
both libraries inside a Debian container and runs a store holder with a
second-process reader for every pairing.

The mutex layouts collide on x86_64, so the container runs as linux/amd64
(emulated on other hosts).

Options:
  --repo-dir <dir>         Repository root (default: inferred)
  --docker-bin <path>      Docker binary to use (default: docker)
  --docker-image <image>   Debian image to use (default: debian:bookworm-slim)
  --platform <platform>    Container platform (default: linux/amd64)
  -h, --help               Show this help
EOF
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"
DOCKER_BIN="${DOCKER_BIN:-docker}"
DOCKER_IMAGE="${DOCKER_IMAGE:-debian:bookworm-slim}"
PLATFORM="${LMDB_CROSS_LIBC_PLATFORM:-linux/amd64}"

while [ $# -gt 0 ]; do
    case "$1" in
        --repo-dir) REPO_DIR="${2:-}"; shift 2 ;;
        --docker-bin) DOCKER_BIN="${2:-}"; shift 2 ;;
        --docker-image) DOCKER_IMAGE="${2:-}"; shift 2 ;;
        --platform) PLATFORM="${2:-}"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown argument: $1" >&2; usage >&2; exit 1 ;;
    esac
done

if ! command -v "$DOCKER_BIN" >/dev/null 2>&1; then
    echo "Missing required command: $DOCKER_BIN" >&2
    exit 1
fi

LMDB_DIR="${REPO_DIR}/rust/vendor/lmdb-master-sys/lmdb/libraries/liblmdb"
TEST_DIR="${REPO_DIR}/rust/tests/lmdb_cross_libc"
for required in "${LMDB_DIR}/mdb.c" "${TEST_DIR}/run_in_container.sh"; do
    if [ ! -f "$required" ]; then
        echo "Missing $required" >&2
        exit 1
    fi
done

# The store lives on the container's own filesystem: shared mappings of the
# lock file are not reliable on host bind mounts.
"$DOCKER_BIN" run --rm --platform "$PLATFORM" \
    -v "${LMDB_DIR}:/lmdb:ro" \
    -v "${TEST_DIR}:/src:ro" \
    "$DOCKER_IMAGE" \
    sh -euc '
        export DEBIAN_FRONTEND=noninteractive
        apt-get update -qq >/dev/null
        apt-get install -y -qq --no-install-recommends gcc libc6-dev musl-tools >/dev/null
        sh /src/run_in_container.sh
    '
