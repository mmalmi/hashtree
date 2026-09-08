#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

grep -F 'default = ["lmdb"]' rust/crates/hashtree-cli/Cargo.toml >/dev/null
grep -F 'fips-webrtc = ["hashtree-fips-transport/webrtc-endpoint"]' rust/crates/hashtree-cli/Cargo.toml >/dev/null
grep -F 'bash scripts/build_release_artifacts.sh' .github/workflows/release.yml >/dev/null
grep -F -- '--linux-builder docker' .github/workflows/release.yml >/dev/null
grep -F 'write_release_bootstrap_installer.sh' .github/workflows/release.yml >/dev/null
if grep -F 'write_signed_release_checksums.sh' .github/workflows/release.yml >/dev/null; then
    echo "release workflow must not publish legacy signed checksums" >&2
    exit 1
fi
if grep -F 'SHA256SUMS' rust/scripts/write_release_bootstrap_installer.sh >/dev/null; then
    echo "release bootstrap must not depend on legacy checksum manifests" >&2
    exit 1
fi
grep -F -- '--device /dev/fuse' rust/scripts/run_fuse_smoke_in_docker.sh >/dev/null
grep -F 'cargo test --locked -p hashtree-cli --features fuse --test fuse_mount_smoke -- --nocapture' rust/scripts/run_fuse_smoke_in_docker.sh >/dev/null

echo "Rust binary feature wiring checks passed."
