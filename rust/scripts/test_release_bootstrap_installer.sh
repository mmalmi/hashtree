#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WRITER="${SCRIPT_DIR}/write_release_bootstrap_installer.sh"

require_command() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "Missing required command: $1" >&2
        exit 1
    fi
}

detect_arch() {
    case "$(uname -m)" in
        arm64|aarch64) printf '%s\n' aarch64 ;;
        x86_64|amd64) printf '%s\n' x86_64 ;;
        *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
    esac
}

detect_os() {
    case "$(uname -s)" in
        Darwin) printf '%s\n' apple-darwin ;;
        Linux) printf '%s\n' unknown-linux-musl ;;
        *) echo "unsupported operating system: $(uname -s)" >&2; exit 1 ;;
    esac
}

require_command curl
require_command python3
require_command tar

PORT="${HASHTREE_TEST_PORT:-$(python3 - <<'PY'
import socket

with socket.socket() as sock:
    sock.bind(("127.0.0.1", 0))
    print(sock.getsockname()[1])
PY
)}"

TMP_DIR="$(mktemp -d)"
cleanup() {
    if [ -n "${SERVER_PID:-}" ]; then
        kill "$SERVER_PID" >/dev/null 2>&1 || true
        wait "$SERVER_PID" >/dev/null 2>&1 || true
    fi
    rm -rf "$TMP_DIR"
}
trap cleanup EXIT

ASSETS_DIR="${TMP_DIR}/assets"
PACKAGE_DIR="${TMP_DIR}/package/hashtree"
mkdir -p "$ASSETS_DIR" "$PACKAGE_DIR"

MARKER="${TMP_DIR}/installed"
cat >"${PACKAGE_DIR}/install.sh" <<'EOF'
#!/bin/sh
set -eu
printf 'installed\n' >"$HASHTREE_TEST_MARKER"
EOF
chmod +x "${PACKAGE_DIR}/install.sh"

target="$(detect_arch)-$(detect_os)"
archive="hashtree-${target}.tar.gz"
(
    cd "${TMP_DIR}/package"
    tar -czf "${ASSETS_DIR}/${archive}" hashtree
)
cp "${ASSETS_DIR}/${archive}" "${TMP_DIR}/${archive}"

"$WRITER" \
    --path "${TMP_DIR}/install.sh" \
    --base-url "http://127.0.0.1:${PORT}"

(
    cd "$TMP_DIR"
    python3 -m http.server "$PORT" --bind 127.0.0.1 >"${TMP_DIR}/http.log" 2>&1
) &
SERVER_PID=$!

for _ in $(seq 1 50); do
    if curl -fsS "http://127.0.0.1:${PORT}/assets/${archive}" >/dev/null 2>&1; then
        break
    fi
    sleep 0.1
done

HASHTREE_TEST_MARKER="$MARKER" sh "${TMP_DIR}/install.sh" >"${TMP_DIR}/good.out" 2>"${TMP_DIR}/good.err"
grep -F "installed" "$MARKER" >/dev/null

"$WRITER" \
    --path "${TMP_DIR}/install-flat.sh" \
    --base-url "http://127.0.0.1:${PORT}/release" \
    --asset-base-url "http://127.0.0.1:${PORT}"

HASHTREE_TEST_MARKER="$MARKER" sh "${TMP_DIR}/install-flat.sh" \
    >"${TMP_DIR}/flat.out" 2>"${TMP_DIR}/flat.err"
grep -F "installed" "$MARKER" >/dev/null
grep -F "ASSET_BASE_URL=\"http://127.0.0.1:${PORT}\"" "${TMP_DIR}/install-flat.sh" >/dev/null

echo "test_release_bootstrap_installer.sh passed"
