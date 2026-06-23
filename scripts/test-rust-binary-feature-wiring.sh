#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

grep -F 'default = ["lmdb"]' rust/crates/hashtree-cli/Cargo.toml >/dev/null
grep -F 'fips-webrtc = ["hashtree-fips-transport/webrtc"]' rust/crates/hashtree-cli/Cargo.toml >/dev/null
grep -F 'bash scripts/run_fuse_smoke_in_docker.sh' .github/workflows/ci.yml >/dev/null
grep -F 'bash scripts/build_release_artifacts.sh' .github/workflows/release.yml >/dev/null
grep -F -- '--linux-builder docker' .github/workflows/release.yml >/dev/null
grep -F 'write_release_bootstrap_installer.sh' .github/workflows/release.yml >/dev/null
! grep -F 'write_signed_release_checksums.sh' .github/workflows/release.yml >/dev/null
! grep -F 'SHA256SUMS' rust/scripts/write_release_bootstrap_installer.sh >/dev/null
grep -F 'Release artifacts omit optional FUSE support' rust/scripts/build_release_artifacts.sh >/dev/null
grep -F 'printf '"'"'%s\n'"'"' ""' rust/scripts/build_release_artifacts.sh >/dev/null
! grep -F 'hashtree-cli/fuse' rust/scripts/build_release_artifacts.sh >/dev/null
! grep -F 'hashtree-cli/fuse' rust/scripts/build_linux_release_target_docker.sh >/dev/null
grep -F -- '--locked' rust/scripts/build_release_artifacts.sh >/dev/null
grep -F -- '--device /dev/fuse' rust/scripts/run_fuse_smoke_in_docker.sh >/dev/null
grep -F 'cargo test -p hashtree-cli --features fuse --test fuse_mount_smoke -- --nocapture' rust/scripts/run_fuse_smoke_in_docker.sh >/dev/null

echo "Rust binary feature wiring checks passed."
