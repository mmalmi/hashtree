#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
mode="full"

case "${1:-}" in
  "") ;;
  --fast) mode="fast" ;;
  -h|--help)
    cat <<'EOF'
Usage: scripts/release-gate.sh [--fast]

Runs the complete pre-publish gate by default. --fast compiles the Rust test
suite without executing it; all TypeScript and release-wiring tests still run.
EOF
    exit 0
    ;;
  *)
    echo "Unknown argument: $1" >&2
    exit 1
    ;;
esac

command -v cargo >/dev/null || { echo "Missing required command: cargo" >&2; exit 1; }
command -v pnpm >/dev/null || { echo "Missing required command: pnpm" >&2; exit 1; }

# Do not inherit a machine-wide Cargo target directory shared by other Codex
# worktrees. Concurrent test jobs must not serialize on or terminate each
# other's artifact locks.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$repo_root/rust/target}"

# The CLI test binary exercises hundreds of LMDB/server fixtures in one
# process. Linux shells commonly default to 1,024 file descriptors, which can
# turn otherwise-passing tests into late, order-dependent EMFILE failures.
ensure_test_fd_limit() {
  local minimum=8192
  local current
  current="$(ulimit -Sn)"
  if [[ "$current" == "unlimited" ]] || { [[ "$current" =~ ^[0-9]+$ ]] && (( current >= minimum )); }; then
    return
  fi
  if ! ulimit -Sn "$minimum" 2>/dev/null; then
    echo "Rust tests require an open-file limit of at least ${minimum} (found ${current})" >&2
    exit 1
  fi
}

git -C "$repo_root" diff --check

bash "$repo_root/scripts/test-release-script-defaults.sh"
bash "$repo_root/scripts/tests/test_publish_release_wrapper.sh"
bash "$repo_root/scripts/test-rust-binary-feature-wiring.sh"
bash "$repo_root/scripts/test-ci-e2e-workflow.sh"
bash "$repo_root/scripts/test-release-gate-wiring.sh"
bash "$repo_root/rust/tests/test_build_release_artifacts.sh"
bash "$repo_root/rust/tests/test_build_release_invocation.sh"
bash "$repo_root/rust/tests/test_build_release_docker_invocation.sh"
node --test "$repo_root/rust/tests/test_build_windows_vm_artifacts.mjs"

pnpm --dir "$repo_root/ts" install --frozen-lockfile
pnpm --dir "$repo_root/ts" test
pnpm --dir "$repo_root/ts" run build:hashtree
pnpm --dir "$repo_root/ts" lint

(
  cd "$repo_root/rust"
  cargo fmt --all --check
  if [ "$mode" = "fast" ]; then
    cargo test --workspace --locked --no-run
  else
    ensure_test_fd_limit
    cargo test --workspace --locked
  fi
)

echo "Hashtree ${mode} release gate passed"
