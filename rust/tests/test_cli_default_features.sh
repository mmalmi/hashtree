#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUST_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

cli_tree="$(cd "${RUST_DIR}" && cargo tree -p hashtree-cli -e features)"
cli_fuse_tree="$(cd "${RUST_DIR}" && cargo tree -p hashtree-cli -e features --features fuse)"
cli_torrent_tree="$(cd "${RUST_DIR}" && cargo tree -p hashtree-cli -e features --features bittorrent)"
cashu_tree="$(cd "${RUST_DIR}" && cargo tree -p hashtree-cashu-cli -e features)"

if printf '%s\n' "$cli_tree" | grep -F 'hashtree-fuse v' >/dev/null; then
    echo "hashtree-cli should not resolve hashtree-fuse by default" >&2
    exit 1
fi

if printf '%s\n' "$cli_tree" | grep -F 'hashtree-bittorrent v' >/dev/null; then
    echo "hashtree-cli must not resolve BitTorrent by default" >&2
    exit 1
fi
printf '%s\n' "$cli_torrent_tree" | grep -F 'hashtree-bittorrent v' >/dev/null || {
    echo "hashtree-cli must resolve BitTorrent when explicitly enabled" >&2
    exit 1
}

printf '%s\n' "$cli_fuse_tree" | grep -F 'hashtree-fuse v' >/dev/null || {
    echo "hashtree-cli should resolve hashtree-fuse when fuse is enabled explicitly" >&2
    exit 1
}

if printf '%s\n' "$cashu_tree" | grep -F 'hashtree-fuse v' >/dev/null; then
    echo "hashtree-cashu-cli should not pull in hashtree-fuse by default" >&2
    exit 1
fi

echo "test_cli_default_features.sh passed"
