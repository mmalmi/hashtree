#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo_root"

reject() {
    if "$@"; then
        printf 'forbidden release-gate condition matched: %s\n' "$*" >&2
        exit 1
    fi
}

bash rust/tests/test_publish_plan.sh

# A clean checkout must resolve every non-workspace Rust dependency without
# sibling repositories mounted beside hashtree.
reject grep -qF 'path = "../../cashu-service/' rust/Cargo.toml
reject grep -qF 'path = "../../../../fips/' rust/crates/hashtree-fips-transport/Cargo.toml
reject grep -qF 'FIPS_DIR' rust/scripts/build_release_artifacts.sh
reject grep -qF 'FIPS_DIR' rust/scripts/build_linux_release_target_docker.sh
reject grep -qF 'for sibling in' rust/scripts/build_linux_release_target_docker.sh
reject grep -qF 'requiredSiblingSourceDirs' rust/scripts/build_windows_vm_artifacts.mjs

# The reusable blob transport has one implementation and does not pull the
# separate Hashtree HTL router into the carrier layer.
grep -F 'default = []' rust/crates/hashtree-fips-transport/Cargo.toml >/dev/null
grep -F 'required-features = ["interop-fixture"]' rust/crates/hashtree-fips-transport/Cargo.toml >/dev/null
grep -F 'webrtc = ["webrtc-endpoint"]' rust/crates/hashtree-fips-transport/Cargo.toml >/dev/null
reject grep -qF 'hashtree-network' rust/crates/hashtree-fips-transport/Cargo.toml

# The normal release path owns one full gate and one artifact publish. Tag
# pushes must not start a second cross-platform build in GitHub Actions.
grep -F '"${REPO_DIR}/scripts/release-gate.sh"' publish_release.sh >/dev/null
grep -F 'export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$repo_root/rust/target}"' scripts/release-gate.sh >/dev/null
grep -F 'ensure_test_fd_limit' scripts/release-gate.sh >/dev/null
grep -F 'cargo nextest run --workspace --locked' scripts/release-gate.sh >/dev/null
grep -F 'run_pool_migration_systemd_gate' scripts/release-gate.sh >/dev/null
grep -F 'cargo build --locked -p hashtree-cli --bin htree' scripts/release-gate.sh >/dev/null
grep -F 'bash tests/test_unified_lmdb_link.sh' scripts/release-gate.sh >/dev/null
grep -F 'exact_offline_stale_pending_cleanup_is_atomic_and_idempotent' scripts/release-gate.sh >/dev/null
grep -F 'cursor_parent_lease_serializes_independent_open_descriptions' scripts/release-gate.sh >/dev/null
grep -F 'cargo test --locked -p hashtree-cli --test pool_migration' scripts/release-gate.sh >/dev/null
grep -F -- '-- --ignored --test-threads=1' scripts/release-gate.sh >/dev/null
grep -F -- '--exclude hashtree-embedded-ffi' scripts/release-gate.sh >/dev/null
grep -F -- '--exclude tauri-plugin-hashtree-updater' scripts/release-gate.sh >/dev/null
grep -F -- '--test-threads 4' scripts/release-gate.sh >/dev/null
grep -F 'taiki-e/install-action@nextest' .github/workflows/ci.yml >/dev/null
grep -F 'rev-parse "${VERSION}^{commit}"' publish_release.sh >/dev/null
reject grep -qF "tags:" .github/workflows/release.yml
grep -F 'gate-static' .github/workflows/release.yml >/dev/null
grep -F 'gate-typescript' .github/workflows/release.yml >/dev/null
grep -F 'gate-rust' .github/workflows/release.yml >/dev/null
grep -F 'gate-rust-peripheral' .github/workflows/release.yml >/dev/null
grep -F 'gate-fips' .github/workflows/release.yml >/dev/null
grep -F 'pool-migration-systemd:' .github/workflows/release.yml >/dev/null
grep -F 'os: ubuntu-24.04-arm' .github/workflows/release.yml >/dev/null
reject grep -qF 'docker/setup-qemu-action' .github/workflows/release.yml
grep -F -- '--asset-base-url "https://github.com/${{ github.repository }}/releases/download/${{ github.event.inputs.tag || github.ref_name }}"' .github/workflows/release.yml >/dev/null
grep -F 'cargo-gate-rust-' .github/workflows/release.yml >/dev/null
grep -F 'cargo-gate-peripheral-' .github/workflows/release.yml >/dev/null
grep -F 'cargo-gate-fips-' .github/workflows/release.yml >/dev/null

# A failing command inside a lane must make the aggregate gate fail even
# though run_all_gates captures each lane's status instead of relying on
# errexit. Bash disables errexit inside a function invoked from an `||` list,
# so exercise the actual aggregate shell with injected command shims.
failure_test_dir="$(mktemp -d "${TMPDIR:-/tmp}/hashtree-release-gate-failure.XXXXXX")"
trap 'rm -rf "$failure_test_dir"' EXIT
failure_bin="$failure_test_dir/bin"
failure_state="$failure_test_dir/failed-nextest"
mkdir -p "$failure_bin"
cat >"$failure_bin/cargo" <<'EOF'
#!/bin/bash
if [[ "${1:-}" == "nextest" && ! -e "$HASHTREE_RELEASE_GATE_FAILURE_STATE" ]]; then
    : >"$HASHTREE_RELEASE_GATE_FAILURE_STATE"
    exit 73
fi
exit 0
EOF
cat >"$failure_bin/success" <<'EOF'
#!/bin/bash
exit 0
EOF
chmod +x "$failure_bin/cargo" "$failure_bin/success"
for command in bash cargo-nextest node pnpm; do
    ln -s success "$failure_bin/$command"
done
if PATH="$failure_bin:$PATH" \
    HASHTREE_RELEASE_GATE_FAILURE_STATE="$failure_state" \
    /bin/bash scripts/release-gate.sh >/dev/null 2>&1; then
    echo "aggregate release gate ignored an injected Rust lane failure" >&2
    exit 1
fi
test -f "$failure_state"

# CI keeps one dependency install and shards the ordinary and FIPS-enabled Rust
# suites onto separate runners.
[ "$(grep -c 'pnpm install --frozen-lockfile' .github/workflows/ci.yml)" -eq 1 ]
reject grep -qF 'cargo test --workspace --tests' .github/workflows/ci.yml
[ "$(grep -Fxc '        run: bash ../scripts/release-gate.sh --lane rust' .github/workflows/ci.yml)" -eq 1 ]
[ "$(grep -Fxc '        run: bash ../scripts/release-gate.sh --lane rust-peripheral' .github/workflows/ci.yml)" -eq 1 ]
[ "$(grep -Fxc '        run: bash ../scripts/release-gate.sh --lane fips' .github/workflows/ci.yml)" -eq 1 ]
[ "$(grep -Fxc '        run: bash scripts/release-gate.sh --lane pool-migration-systemd' .github/workflows/ci.yml)" -eq 1 ]
grep -F 'rust-fuse-smoke:' .github/workflows/ci.yml >/dev/null
[ "$(grep -Fxc '        run: bash rust/scripts/run_fuse_smoke_in_docker.sh' .github/workflows/ci.yml)" -eq 1 ]
[ "$(grep -h 'libwebkit2gtk-4.1-dev' .github/workflows/ci.yml .github/workflows/release.yml | wc -l | tr -d ' ')" -eq 2 ]
[ "$(grep -Fc 'libdbus-1-dev' .github/workflows/ci.yml)" -eq 5 ]
[ "$(grep -Fc 'libdbus-1-dev' .github/workflows/release.yml)" -eq 5 ]

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
grep -F -- '--features hashtree-cli/fips-webrtc' .github/workflows/release.yml >/dev/null
grep -F 'smoke_release_webrtc_unix.sh' .github/workflows/release.yml >/dev/null
grep -F 'WebRTC transport started' rust/scripts/smoke_release_webrtc_windows.ps1 >/dev/null

# No Svelte component exists in this TypeScript workspace, so loading the
# Svelte lint plugin only adds an undeclared peer and makes clean CI installs
# fail before linting the actual sources.
if find ts -type f -name '*.svelte' -print -quit | grep -q .; then
    echo 'Svelte source unexpectedly exists in the TypeScript workspace' >&2
    exit 1
fi
reject grep -qF "eslint-plugin-svelte" ts/package.json ts/eslint.config.js
reject grep -qF "svelte-eslint-parser" ts/package.json ts/eslint.config.js

echo "release gate wiring checks passed"
