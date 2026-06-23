#!/bin/bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: rust/scripts/write_signed_release_checksums.sh --dir <release-asset-dir> --private-key-file <path>

Writes SHA256SUMS and SHA256SUMS.sig for release archive assets. The signature
uses `openssl dgst -sha256 -sign` so install bootstraps can verify the manifest
with a pinned public key before trusting archive checksums.
EOF
}

ASSET_DIR=""
PRIVATE_KEY_FILE=""

while [ $# -gt 0 ]; do
    case "$1" in
        --dir)
            ASSET_DIR="${2:-}"
            shift 2
            ;;
        --private-key-file)
            PRIVATE_KEY_FILE="${2:-}"
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

if [ -z "$ASSET_DIR" ] || [ -z "$PRIVATE_KEY_FILE" ]; then
    usage >&2
    exit 1
fi

if [ ! -d "$ASSET_DIR" ]; then
    echo "Missing release asset directory: $ASSET_DIR" >&2
    exit 1
fi

if [ ! -f "$PRIVATE_KEY_FILE" ]; then
    echo "Missing private key file: $PRIVATE_KEY_FILE" >&2
    exit 1
fi

require_command() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "Missing required command: $1" >&2
        exit 1
    fi
}

sha256_file() {
    local file_path="$1"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$file_path" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$file_path" | awk '{print $1}'
    else
        openssl dgst -sha256 -r "$file_path" | awk '{print $1}'
    fi
}

require_command awk
require_command find
require_command openssl
require_command sort

tmp_sums="$(mktemp)"
cleanup() {
    rm -f "$tmp_sums"
}
trap cleanup EXIT

(
    cd "$ASSET_DIR"
    find . -maxdepth 1 -type f \( -name 'hashtree-*.tar.gz' -o -name 'hashtree-*.zip' \) \
        | sed 's|^\./||' \
        | sort \
        | while IFS= read -r asset; do
            digest="$(sha256_file "$asset")"
            printf '%s  %s\n' "$digest" "$asset"
        done
) >"$tmp_sums"

if [ ! -s "$tmp_sums" ]; then
    echo "No release archives found in $ASSET_DIR" >&2
    exit 1
fi

mv "$tmp_sums" "${ASSET_DIR}/SHA256SUMS"
openssl dgst -sha256 -sign "$PRIVATE_KEY_FILE" -out "${ASSET_DIR}/SHA256SUMS.sig" "${ASSET_DIR}/SHA256SUMS"
