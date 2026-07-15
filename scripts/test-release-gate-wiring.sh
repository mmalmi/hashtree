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

# The reusable same-host store stays independently consumable without pulling
# in or falling back to the old mesh protocol. The CLI opts into compatibility
# explicitly until its broader mesh/pubsub surface migrates.
grep -F 'default = []' rust/crates/hashtree-fips-transport/Cargo.toml >/dev/null
grep -F 'hashtree-network = { workspace = true, optional = true }' rust/crates/hashtree-fips-transport/Cargo.toml >/dev/null
grep -F 'required-features = ["interop-fixture"]' rust/crates/hashtree-fips-transport/Cargo.toml >/dev/null
grep -F 'hashtree-fips-transport = { workspace = true, features = ["legacy-mesh"] }' rust/crates/hashtree-cli/Cargo.toml >/dev/null

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

# The independently consumable FIPS transport has one canonical TypeScript
# verification command. Both CI and the release gate must invoke it instead of
# copying its build/test/lint sequence into multiple release paths.
node --input-type=module <<'NODE'
import fs from 'node:fs';

const packageJson = JSON.parse(fs.readFileSync('ts/package.json', 'utf8'));
const transportPackage = JSON.parse(
  fs.readFileSync('ts/packages/hashtree-fips-transport/package.json', 'utf8'),
);
const ci = fs.readFileSync('.github/workflows/ci.yml', 'utf8');
const expected = [
  'pnpm --filter @hashtree/fips-transport... build',
  'pnpm --filter @hashtree/fips-transport verify:dist',
  'pnpm --filter @hashtree/fips-transport test',
  'pnpm --filter @hashtree/fips-transport lint',
].join(' && ');

if (packageJson.scripts?.['verify:fips-transport'] !== expected) {
  throw new Error('ts/package.json must define the canonical FIPS transport gate');
}
if (transportPackage.scripts?.['verify:dist'] !== 'node ../../scripts/assert-clean.mjs dist') {
  throw new Error('the FIPS transport gate must reject stale generated files');
}
const typescriptJob = ci.slice(ci.indexOf('  typescript:'), ci.indexOf('  rust-tests:'));
if (!typescriptJob.includes('dtolnay/rust-toolchain@stable')) {
  throw new Error('the TypeScript job must provision Rust for the interop test');
}
NODE
[ "$(grep -Fc 'pnpm run verify:fips-transport' .github/workflows/ci.yml)" -eq 1 ]
[ "$(grep -Fc 'pnpm --dir "$repo_root/ts" run verify:fips-transport' scripts/release-gate.sh)" -eq 1 ]

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
