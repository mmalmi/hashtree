#!/usr/bin/env bash
set -euo pipefail

rust_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
smoke_script="$rust_dir/scripts/smoke_release_webrtc_unix.sh"
tmpdir="$(mktemp -d)"
cleanup() {
    rm -rf "$tmpdir"
}
trap cleanup EXIT

mkdir -p "$tmpdir/good/hashtree" "$tmpdir/bad/hashtree"

printf '%s\n' \
    '#!/usr/bin/env bash' \
    'echo "WebRTC transport started with FIPS session signaling" >&2' \
    'echo "Transports initialized count=5" >&2' \
    'exit 1' >"$tmpdir/good/hashtree/htree"
chmod +x "$tmpdir/good/hashtree/htree"
tar -czf "$tmpdir/good.tar.gz" -C "$tmpdir/good" hashtree
"$smoke_script" "$tmpdir/good.tar.gz" >/dev/null

printf '%s\n' \
    '#!/usr/bin/env bash' \
    'echo "FIPS WebRTC transport requested but this binary was built without the webrtc feature" >&2' \
    'exit 1' >"$tmpdir/bad/hashtree/htree"
chmod +x "$tmpdir/bad/hashtree/htree"
tar -czf "$tmpdir/bad.tar.gz" -C "$tmpdir/bad" hashtree
if "$smoke_script" "$tmpdir/bad.tar.gz" >/dev/null 2>&1; then
    echo "WebRTC smoke accepted an artifact without WebRTC support" >&2
    exit 1
fi

echo "test_release_webrtc_smoke.sh passed"
