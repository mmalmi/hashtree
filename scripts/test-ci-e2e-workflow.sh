#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

grep -F 'HTREE_E2E_RUST_TARGET_DIR: ${{ github.workspace }}/rust/target' .github/workflows/ci.yml >/dev/null
grep -F 'cargo build --manifest-path ../rust/Cargo.toml -p hashtree-cli --bin htree' .github/workflows/ci.yml >/dev/null
grep -F 'cargo build --manifest-path ../rust/Cargo.toml --release -p hashtree-cli --bin htree' .github/workflows/ci.yml >/dev/null
grep -F 'cargo build --manifest-path ../rust/Cargo.toml --release -p git-remote-htree' .github/workflows/ci.yml >/dev/null
grep -F 'timeout-minutes: 75' .github/workflows/ci.yml >/dev/null
grep -F 'name: Rust E2E Smoke Tests' .github/workflows/ci.yml >/dev/null
grep -F 'libdbus-1-dev' .github/workflows/ci.yml >/dev/null
grep -F 'cargo test --manifest-path ../rust/Cargo.toml -p hashtree-network --test formal_mesh_props' .github/workflows/ci.yml >/dev/null
grep -F 'pnpm --filter @hashtree/collection build' .github/workflows/formal-verify.yml >/dev/null
grep -F 'pnpm --filter @hashtree/mesh build' .github/workflows/formal-verify.yml >/dev/null

echo "CI E2E workflow checks passed."
