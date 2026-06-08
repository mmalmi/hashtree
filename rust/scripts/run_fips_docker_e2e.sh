#!/bin/bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: rust/scripts/run_fips_docker_e2e.sh [options]

Build a cached multi-stage htree runtime image, then run a Docker e2e with two
htree daemon containers connected through FIPS discovery and UDP transport.

Options:
  --repo-dir <dir>             Hashtree repository root (default: inferred)
  --fips-dir <dir>             FIPS repository root (default: ../fips)
  --docker-bin <path>          Docker binary to use (default: docker)
  --docker-rust-image <image>  Rust Debian image used to build test binaries
  --keep-workdir               Keep the temporary work directory after exit
  -h, --help                   Show this help
EOF
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUST_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
DEFAULT_REPO_DIR="$(cd "${RUST_DIR}/.." && pwd)"
DEFAULT_FIPS_DIR="$(cd "${DEFAULT_REPO_DIR}/../fips" 2>/dev/null && pwd || true)"

REPO_DIR="${DEFAULT_REPO_DIR}"
FIPS_DIR="${DEFAULT_FIPS_DIR}"
DOCKER_BIN="${DOCKER_BIN:-docker}"
DOCKER_RUST_IMAGE="${DOCKER_RUST_IMAGE:-rust:1.94.1-bookworm}"
KEEP_WORKDIR=0

while [ $# -gt 0 ]; do
    case "$1" in
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
        --keep-workdir)
            KEEP_WORKDIR=1
            shift
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

require_command() {
    local cmd="$1"
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "Missing required command: $cmd" >&2
        exit 1
    fi
}

require_command "$DOCKER_BIN"
REPO_DIR="$(cd "$REPO_DIR" && pwd)"
if [ -z "$FIPS_DIR" ] || [ ! -d "$FIPS_DIR/crates/fips-core" ]; then
    echo "FIPS repo not found. Pass --fips-dir /path/to/fips." >&2
    exit 1
fi
FIPS_DIR="$(cd "$FIPS_DIR" && pwd)"

RUN_ID="htree-fips-e2e-$(date +%s)-$$"
WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/${RUN_ID}.XXXXXX")"
IMAGE="${RUN_ID}:runtime"
NETWORK="${RUN_ID}"
RELAY="${RUN_ID}-relay"
NODE_A="${RUN_ID}-a"
NODE_B="${RUN_ID}-b"
NET_OCTET="${HTREE_FIPS_E2E_NET_OCTET:-$(( (RANDOM % 200) + 20 ))}"
SUBNET="198.19.${NET_OCTET}.0/24"
RELAY_IP="198.19.${NET_OCTET}.2"
NODE_A_IP="198.19.${NET_OCTET}.10"
NODE_B_IP="198.19.${NET_OCTET}.11"
SCOPE="hashtree-v1-docker-${RUN_ID}"
PAYLOAD="hashtree/fips docker e2e ${RUN_ID}"
DOCKER_RUN_USER="${HTREE_FIPS_E2E_UID:-$(id -u)}:${HTREE_FIPS_E2E_GID:-$(id -g)}"

cleanup() {
    set +e
    "$DOCKER_BIN" rm -f "$NODE_A" "$NODE_B" "$RELAY" >/dev/null 2>&1
    "$DOCKER_BIN" network rm "$NETWORK" >/dev/null 2>&1
    "$DOCKER_BIN" image rm "$IMAGE" >/dev/null 2>&1
    if [ "$KEEP_WORKDIR" -eq 0 ]; then
        rm -rf "$WORKDIR"
    else
        echo "Kept workdir: $WORKDIR"
    fi
}
trap cleanup EXIT

show_logs() {
    set +e
    for name in "$RELAY" "$NODE_A" "$NODE_B"; do
        echo
        echo "===== docker logs ${name} ====="
        "$DOCKER_BIN" logs --tail 200 "$name" 2>&1 || true
    done
}

mkdir -p \
    "$WORKDIR/a/config" "$WORKDIR/a/data" \
    "$WORKDIR/b/config" "$WORKDIR/b/data"
printf '%s\n' "$PAYLOAD" >"$WORKDIR/payload.txt"

echo "Building cached runtime image ${IMAGE}..."
env DOCKER_BUILDKIT="${DOCKER_BUILDKIT:-1}" "$DOCKER_BIN" build \
    -f "${RUST_DIR}/Dockerfile.fips-e2e" \
    --build-arg "RUST_IMAGE=${DOCKER_RUST_IMAGE}" \
    --build-context "hashtree-rust=${REPO_DIR}/rust" \
    --build-context "fips=${FIPS_DIR}" \
    -t "$IMAGE" \
    "${RUST_DIR}"

write_node_config() {
    local config_dir="$1"
    local external_addr="$2"

    cat >"${config_dir}/config.toml" <<EOF
[server]
bind_address = "0.0.0.0:8080"
enable_auth = false
mode = "normal"
stun_port = 0
enable_webrtc = false
http_webrtc_fetch = false
enable_multicast = false
max_multicast_peers = 0
enable_wifi_aware = false
max_wifi_aware_peers = 0
enable_bluetooth = false
max_bluetooth_peers = 0
public_writes = true
enable_fips = true
fips_discovery_scope = "${SCOPE}"
fips_relays = ["ws://${RELAY}:7777"]
enable_fips_udp = true
fips_udp_bind_addr = "0.0.0.0:2121"
fips_udp_public = true
fips_udp_external_addr = "${external_addr}:2121"
enable_fips_webrtc = false
fetch_from_fips_peers = true
fips_request_timeout_ms = 20000

[storage]
data_dir = "/data"
max_size_gb = 1
evict_orphans = false

[nostr]
enabled = false
relays = []
bootstrap_follows = []

[blossom]
enabled = false
servers = []
read_servers = []
write_servers = []
require_random_untrusted_ingest = false

[sync]
enabled = false
EOF
}

write_node_config "$WORKDIR/a/config" "$NODE_A_IP"
write_node_config "$WORKDIR/b/config" "$NODE_B_IP"

echo "Creating Docker network ${NETWORK} (${SUBNET})..."
"$DOCKER_BIN" network create --subnet "$SUBNET" "$NETWORK" >/dev/null

echo "Starting FIPS relay and htree daemons..."
"$DOCKER_BIN" run -d --name "$RELAY" --user "$DOCKER_RUN_USER" --network "$NETWORK" --ip "$RELAY_IP" \
    -e HOME=/tmp \
    "$IMAGE" nostr-relay-smoke --addr 0.0.0.0:7777 >/dev/null

"$DOCKER_BIN" run -d --name "$NODE_A" --user "$DOCKER_RUN_USER" --network "$NETWORK" --ip "$NODE_A_IP" \
    -e HOME=/tmp \
    -e HTREE_CONFIG_DIR=/config \
    -e HTREE_DATA_DIR=/data \
    -e HTREE_ALLOW_ROOT_DAEMON=1 \
    -e RUST_LOG="${RUST_LOG:-info}" \
    -v "${WORKDIR}/a/config:/config" \
    -v "${WORKDIR}/a/data:/data" \
    -v "${WORKDIR}/payload.txt:/payload.txt:ro" \
    "$IMAGE" htree start --addr 0.0.0.0:8080 >/dev/null

"$DOCKER_BIN" run -d --name "$NODE_B" --user "$DOCKER_RUN_USER" --network "$NETWORK" --ip "$NODE_B_IP" \
    -e HOME=/tmp \
    -e HTREE_CONFIG_DIR=/config \
    -e HTREE_DATA_DIR=/data \
    -e HTREE_ALLOW_ROOT_DAEMON=1 \
    -e RUST_LOG="${RUST_LOG:-info}" \
    -v "${WORKDIR}/b/config:/config" \
    -v "${WORKDIR}/b/data:/data" \
    "$IMAGE" htree start --addr 0.0.0.0:8080 >/dev/null

wait_for_health() {
    local container="$1"
    for _ in $(seq 1 60); do
        if "$DOCKER_BIN" exec "$container" curl -fsS http://127.0.0.1:8080/health >/dev/null 2>&1; then
            return 0
        fi
        sleep 1
    done
    echo "Timed out waiting for ${container} health" >&2
    show_logs
    exit 1
}

wait_for_fips_peer() {
    local container="$1"
    for _ in $(seq 1 90); do
        if "$DOCKER_BIN" exec "$container" sh -lc \
            'curl -fsS http://127.0.0.1:8080/api/status | jq -e ".fips.enabled == true and .fips.total_peers >= 1" >/dev/null' \
            >/dev/null 2>&1; then
            return 0
        fi
        sleep 1
    done
    echo "Timed out waiting for ${container} to discover a FIPS peer" >&2
    "$DOCKER_BIN" exec "$container" sh -lc 'curl -fsS http://127.0.0.1:8080/api/status || true' >&2 || true
    show_logs
    exit 1
}

wait_for_health "$NODE_A"
wait_for_health "$NODE_B"

echo "Adding payload on node A..."
ADD_OUTPUT="$("$DOCKER_BIN" exec "$NODE_A" sh -lc 'HTREE_CONFIG_DIR=/config HTREE_DATA_DIR=/data htree add /payload.txt --unencrypted --local' 2>&1)"
printf '%s\n' "$ADD_OUTPUT"
CID="$(printf '%s\n' "$ADD_OUTPUT" | grep -Eo '[a-f0-9]{64}' | head -n1 || true)"
if [ -z "$CID" ]; then
    echo "Failed to extract payload hash from htree add output" >&2
    show_logs
    exit 1
fi

echo "Waiting for FIPS peer discovery..."
wait_for_fips_peer "$NODE_A"
wait_for_fips_peer "$NODE_B"

echo "Checking node A can serve the local payload..."
"$DOCKER_BIN" exec "$NODE_A" curl -fsS "http://127.0.0.1:8080/${CID}" >"${WORKDIR}/from-a.txt"
if ! cmp -s "$WORKDIR/payload.txt" "$WORKDIR/from-a.txt"; then
    echo "Node A returned unexpected payload bytes" >&2
    show_logs
    exit 1
fi

echo "Fetching payload from node B over Hashtree/FIPS..."
"$DOCKER_BIN" exec "$NODE_B" curl -fsS --max-time 30 "http://127.0.0.1:8080/${CID}" >"${WORKDIR}/from-b.txt"
if ! cmp -s "$WORKDIR/payload.txt" "$WORKDIR/from-b.txt"; then
    echo "Node B returned unexpected payload bytes" >&2
    show_logs
    exit 1
fi

echo "Verifying node B cached the fetched blob..."
"$DOCKER_BIN" exec "$NODE_B" curl -fsS "http://127.0.0.1:8080/${CID}" >"${WORKDIR}/from-b-cached.txt"
cmp -s "$WORKDIR/payload.txt" "$WORKDIR/from-b-cached.txt"

echo "FIPS Docker e2e passed: node B fetched ${CID} from node A with legacy transports disabled."
