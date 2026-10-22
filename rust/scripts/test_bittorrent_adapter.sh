#!/usr/bin/env bash
set -euo pipefail

rust_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$rust_root"
export HASHTREE_BITTORRENT_PYTHON="${HASHTREE_BITTORRENT_PYTHON:-python3}"
log_dir="${HASHTREE_BITTORRENT_LOG_DIR:-$(mktemp -d "${TMPDIR:-/tmp}/hashtree-bittorrent.XXXXXX")}"
mkdir -p "$log_dir"

pkg-config --atleast-version=2.1 libtorrent-rasterbar
pkg-config --max-version=2.1.99 libtorrent-rasterbar
"$HASHTREE_BITTORRENT_PYTHON" -c 'import libtorrent'

run() {
    local name="$1"
    shift
    if "$@" >"$log_dir/$name.log" 2>&1; then
        printf '%s passed (%s)\n' "$name" "$log_dir/$name.log"
    else
        local result=$?
        tail -60 "$log_dir/$name.log"
        printf '%s failed with exit %s; full log: %s\n' "$name" "$result" "$log_dir/$name.log" >&2
        return "$result"
    fi
}

run formatting cargo fmt --all --check
run adapter-default cargo test --locked -p hashtree-bittorrent --no-default-features --test snapshot
run adapter cargo test --locked -p hashtree-bittorrent --features libtorrent -- --test-threads=1
run cli-enabled cargo test --locked -p hashtree-cli --bin htree --features bittorrent torrent_ -- --test-threads=1
run cli-default cargo test --locked -p hashtree-cli --bin htree torrent_command_is_strictly_feature_gated
run dependencies bash tests/test_cli_default_features.sh
run build cargo build --locked -p hashtree-cli --bin htree --features bittorrent
target_dir="$(cargo metadata --no-deps --format-version=1 | "$HASHTREE_BITTORRENT_PYTHON" -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
run cli-roundtrip "$HASHTREE_BITTORRENT_PYTHON" crates/hashtree-bittorrent/tests/cli_roundtrip.py "$target_dir/debug/htree"
printf 'BitTorrent optional gate passed; logs: %s\n' "$log_dir"
