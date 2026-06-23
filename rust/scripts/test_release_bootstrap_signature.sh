#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"
WRITER="${SCRIPT_DIR}/write_release_bootstrap_installer.sh"
SIGNER="${SCRIPT_DIR}/write_signed_release_checksums.sh"

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
require_command openssl
require_command python3
require_command tar

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

openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out "${TMP_DIR}/private.pem" >/dev/null 2>&1
openssl pkey -in "${TMP_DIR}/private.pem" -pubout -out "${TMP_DIR}/public.pem" >/dev/null 2>&1
"$SIGNER" --dir "$ASSETS_DIR" --private-key-file "${TMP_DIR}/private.pem"
cp "${ASSETS_DIR}/SHA256SUMS.sig" "${TMP_DIR}/SHA256SUMS.sig.good"

"$WRITER" \
    --path "${TMP_DIR}/install.sh" \
    --base-url "http://127.0.0.1:8765" \
    --asset-base-url "http://127.0.0.1:8765" \
    --public-key-file "${TMP_DIR}/public.pem"

(
    cd "$ASSETS_DIR"
    python3 -m http.server 8765 --bind 127.0.0.1 >"${TMP_DIR}/http.log" 2>&1
) &
SERVER_PID=$!

for _ in $(seq 1 50); do
    if curl -fsS "http://127.0.0.1:8765/SHA256SUMS" >/dev/null 2>&1; then
        break
    fi
    sleep 0.1
done

printf 'bad signature\n' >"${ASSETS_DIR}/SHA256SUMS.sig"
if HASHTREE_TEST_MARKER="$MARKER" sh "${TMP_DIR}/install.sh" >"${TMP_DIR}/bad.out" 2>"${TMP_DIR}/bad.err"; then
    echo "bootstrap accepted a bad release manifest signature" >&2
    exit 1
fi
grep -F "signature verification failed" "${TMP_DIR}/bad.err" >/dev/null
[ ! -e "$MARKER" ] || { echo "packaged installer ran after bad signature" >&2; exit 1; }

cp "${TMP_DIR}/SHA256SUMS.sig.good" "${ASSETS_DIR}/SHA256SUMS.sig"
HASHTREE_TEST_MARKER="$MARKER" sh "${TMP_DIR}/install.sh" >"${TMP_DIR}/good.out" 2>"${TMP_DIR}/good.err"
grep -F "installed" "$MARKER" >/dev/null

echo "test_release_bootstrap_signature.sh passed"
