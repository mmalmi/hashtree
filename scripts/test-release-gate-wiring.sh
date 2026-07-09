#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

# A clean checkout must resolve every non-workspace Rust dependency without
# sibling repositories mounted beside hashtree.
! grep -F 'path = "../../cashu-service/' rust/Cargo.toml >/dev/null
! grep -F 'path = "../../../../fips/' rust/crates/hashtree-fips-transport/Cargo.toml >/dev/null
! grep -F 'FIPS_DIR' rust/scripts/build_release_artifacts.sh >/dev/null
! grep -F 'FIPS_DIR' rust/scripts/build_linux_release_target_docker.sh >/dev/null
! grep -F 'for sibling in' rust/scripts/build_linux_release_target_docker.sh >/dev/null
! grep -F 'requiredSiblingSourceDirs' rust/scripts/build_windows_vm_artifacts.mjs >/dev/null

# The normal release path owns one full gate and one artifact publish. Tag
# pushes must not start a second cross-platform build in GitHub Actions.
grep -F '"${REPO_DIR}/scripts/release-gate.sh"' publish_release.sh >/dev/null
grep -F 'export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$repo_root/rust/target}"' scripts/release-gate.sh >/dev/null
grep -F 'ensure_test_fd_limit' scripts/release-gate.sh >/dev/null
grep -F 'ulimit -Sn 8192' .github/workflows/ci.yml >/dev/null
grep -F 'rev-parse "${VERSION}^{commit}"' publish_release.sh >/dev/null
! grep -F "tags:" .github/workflows/release.yml >/dev/null
grep -F 'needs: gate' .github/workflows/release.yml >/dev/null

# CI keeps the same coverage while avoiding duplicate dependency installs and
# a second `cargo test` execution that already overlaps the workspace suite.
[ "$(grep -c 'pnpm install --frozen-lockfile' .github/workflows/ci.yml)" -eq 1 ]
[ "$(grep -Ec 'cargo test --workspace( --locked)?$' .github/workflows/ci.yml)" -eq 1 ]
! grep -F 'cargo test --workspace --tests' .github/workflows/ci.yml >/dev/null
[ "$(grep -h 'libwebkit2gtk-4.1-dev' .github/workflows/ci.yml .github/workflows/release.yml | wc -l | tr -d ' ')" -eq 2 ]

# Windows packaging must build every binary before copying it into the zip.
grep -F 'name: Build release binaries (Windows)' .github/workflows/release.yml >/dev/null
grep -F -- '-p git-remote-htree' .github/workflows/release.yml >/dev/null
grep -F -- '-p hashtree-cashu-cli' .github/workflows/release.yml >/dev/null
grep -F -- '-p hashtree-cli' .github/workflows/release.yml >/dev/null

# No Svelte component exists in this TypeScript workspace, so loading the
# Svelte lint plugin only adds an undeclared peer and makes clean CI installs
# fail before linting the actual sources.
! find ts -type f -name '*.svelte' -print -quit | grep -q .
! grep -F "eslint-plugin-svelte" ts/package.json ts/eslint.config.js >/dev/null
! grep -F "svelte-eslint-parser" ts/package.json ts/eslint.config.js >/dev/null

echo "release gate wiring checks passed"
