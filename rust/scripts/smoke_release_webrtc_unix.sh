#!/usr/bin/env bash
set -euo pipefail

archive="${1:-}"
if [ -z "$archive" ] || [ ! -f "$archive" ]; then
    echo "Usage: $0 <hashtree-target.tar.gz>" >&2
    exit 2
fi

smoke_dir="$(mktemp -d)"
cleanup() {
    rm -rf "$smoke_dir"
}
trap cleanup EXIT

tar -xzf "$archive" -C "$smoke_dir"
binary="$smoke_dir/hashtree/htree"
if [ ! -x "$binary" ]; then
    echo "Release archive does not contain an executable hashtree/htree" >&2
    exit 1
fi

mkdir -p "$smoke_dir/config" "$smoke_dir/data" "$smoke_dir/home"
set +e
output="$({
    env \
        HOME="$smoke_dir/home" \
        HTREE_CONFIG_DIR="$smoke_dir/config" \
        RUST_LOG='fips_core::transport::webrtc=info,fips_core::node::lifecycle::runtime=info' \
        "$binary" --data-dir "$smoke_dir/data" start \
            --addr 127.0.0.1:0 --relays ''
} 2>&1)"
set -e
printf '%s\n' "$output"

if ! grep -F 'WebRTC transport started' <<<"$output" >/dev/null; then
    echo "Packaged htree did not start its FIPS WebRTC transport" >&2
    exit 1
fi
if grep -F 'built without the webrtc feature' <<<"$output" >/dev/null; then
    echo "Packaged htree lacks the required FIPS WebRTC feature" >&2
    exit 1
fi

echo "Packaged FIPS WebRTC startup smoke passed"
