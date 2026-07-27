#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
mode="full"
lane="all"

usage() {
  cat <<'EOF'
Usage: scripts/release-gate.sh [--fast] [--lane static|typescript|rust|rust-peripheral|fips]

Runs the complete pre-publish gate by default. Independent static and
TypeScript checks overlap the main Rust lane; dependency-heavy peripheral and
FIPS WebRTC Rust packages use independent lanes. --fast compiles Rust tests
without executing them. --lane runs one CI-shardable portion of the gate.
EOF
}

while (( $# > 0 )); do
  case "$1" in
    --fast) mode="fast" ;;
    --lane)
      shift
      lane="${1:-}"
      if [[ -z "$lane" ]]; then
        echo "--lane requires a value" >&2
        exit 1
      fi
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
  shift
done

case "$lane" in
  all|static|typescript|rust|rust-peripheral|fips) ;;
  *)
    echo "Unknown release gate lane: $lane" >&2
    exit 1
    ;;
esac

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
    return 1
  fi
}

require_rust_tools() {
  command -v cargo >/dev/null || {
    echo "Missing required command: cargo" >&2
    return 1
  }
  command -v cargo-nextest >/dev/null || {
    echo "Missing required command: cargo-nextest (install from https://nexte.st)" >&2
    return 1
  }
}

run_static_gate() {
  git -C "$repo_root" diff --check || return $?

  bash "$repo_root/scripts/test-release-script-defaults.sh" || return $?
  bash "$repo_root/scripts/tests/test_publish_release_wrapper.sh" || return $?
  bash "$repo_root/scripts/test-rust-binary-feature-wiring.sh" || return $?
  bash "$repo_root/scripts/test-release-gate-wiring.sh" || return $?
  bash "$repo_root/rust/tests/test_build_release_artifacts.sh" || return $?
  bash "$repo_root/rust/tests/test_build_release_invocation.sh" || return $?
  bash "$repo_root/rust/tests/test_build_release_docker_invocation.sh" || return $?
  bash "$repo_root/rust/tests/test_release_webrtc_smoke.sh" || return $?
  node --test "$repo_root/rust/tests/test_build_windows_vm_artifacts.mjs" || return $?
}

run_typescript_gate() {
  command -v pnpm >/dev/null || {
    echo "Missing required command: pnpm" >&2
    return 1
  }
  pnpm --dir "$repo_root/ts" install --frozen-lockfile || return $?
  pnpm --dir "$repo_root/ts" test || return $?
  pnpm --dir "$repo_root/ts" run verify:fips-transport || return $?
  pnpm --dir "$repo_root/ts" lint || return $?
}

run_rust_gate() {
  require_rust_tools || return $?
  (
    cd "$repo_root/rust" || exit $?
    cargo fmt --all --check || exit $?
    if [[ "$mode" == "fast" ]]; then
      cargo nextest run --workspace --locked \
        --exclude hashtree-embedded \
        --exclude hashtree-embedded-ffi \
        --exclude hashtree-s3 \
        --exclude hashtree-ffi \
        --exclude hashtree-cashu-cli \
        --exclude tauri-plugin-hashtree-updater \
        --no-run || exit $?
    else
      ensure_test_fd_limit || exit $?
      cargo nextest run --workspace --locked \
        --exclude hashtree-embedded \
        --exclude hashtree-embedded-ffi \
        --exclude hashtree-s3 \
        --exclude hashtree-ffi \
        --exclude hashtree-cashu-cli \
        --exclude tauri-plugin-hashtree-updater || exit $?
      # These are the only workspace crates with executable doctests.
      cargo test --locked -p hashtree-core -p hashtree-blossom --doc || exit $?
    fi
  )
}

run_rust_peripheral_gate() {
  require_rust_tools || return $?
  (
    cd "$repo_root/rust" || exit $?
    if [[ "$mode" == "fast" ]]; then
      cargo nextest run --locked \
        -p hashtree-s3 \
        -p hashtree-ffi \
        -p hashtree-cashu-cli \
        -p tauri-plugin-hashtree-updater \
        --no-run || exit $?
    else
      ensure_test_fd_limit || exit $?
      cargo nextest run --locked \
        -p hashtree-s3 \
        -p hashtree-ffi \
        -p hashtree-cashu-cli \
        -p tauri-plugin-hashtree-updater || exit $?
    fi
  )
}

run_fips_gate() {
  require_rust_tools || return $?
  (
    cd "$repo_root/rust" || exit $?
    if [[ "$mode" == "fast" ]]; then
      cargo nextest run --locked \
        -p hashtree-embedded \
        -p hashtree-embedded-ffi \
        --test-threads 4 \
        --no-run || exit $?
    else
      ensure_test_fd_limit || exit $?
      cargo nextest run --locked \
        -p hashtree-embedded \
        -p hashtree-embedded-ffi \
        --test-threads 4 || exit $?
    fi
  )
}

run_all_gates() {
  local log_dir static_pid typescript_pid peripheral_pid rust_status static_status
  local typescript_status peripheral_status fips_status
  log_dir="$(mktemp -d "${TMPDIR:-/tmp}/hashtree-release-gate.XXXXXX")"

  run_static_gate >"$log_dir/static.log" 2>&1 &
  static_pid=$!
  run_typescript_gate >"$log_dir/typescript.log" 2>&1 &
  typescript_pid=$!

  rust_status=0
  run_rust_gate || rust_status=$?
  static_status=0
  wait "$static_pid" || static_status=$?
  typescript_status=0
  wait "$typescript_pid" || typescript_status=$?

  cat "$log_dir/static.log"
  cat "$log_dir/typescript.log"

  if (( rust_status != 0 || static_status != 0 || typescript_status != 0 )); then
    echo "Release gate lane failed (rust=$rust_status static=$static_status typescript=$typescript_status)" >&2
    rm -rf "$log_dir"
    return 1
  fi

  run_rust_peripheral_gate >"$log_dir/rust-peripheral.log" 2>&1 &
  peripheral_pid=$!
  fips_status=0
  run_fips_gate || fips_status=$?
  peripheral_status=0
  wait "$peripheral_pid" || peripheral_status=$?
  cat "$log_dir/rust-peripheral.log"
  rm -rf "$log_dir"
  if (( fips_status != 0 || peripheral_status != 0 )); then
    echo "Release gate lane failed (fips=$fips_status rust-peripheral=$peripheral_status)" >&2
    return 1
  fi
}

case "$lane" in
  all) run_all_gates ;;
  static) run_static_gate ;;
  typescript) run_typescript_gate ;;
  rust) run_rust_gate ;;
  rust-peripheral) run_rust_peripheral_gate ;;
  fips) run_fips_gate ;;
esac

echo "Hashtree ${mode} release gate (${lane}) passed"
