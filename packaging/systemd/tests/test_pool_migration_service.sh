#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SYSTEMD_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
UNIT="${SYSTEMD_DIR}/hashtree-pool-migration-worker@.service"
CONTROLLER_UNIT="${SYSTEMD_DIR}/hashtree-pool-migration-controller@.service"
README="${SYSTEMD_DIR}/README.md"

test -f "$UNIT"
test -f "$CONTROLLER_UNIT"
grep -Fx 'Type=exec' "$CONTROLLER_UNIT" >/dev/null
grep -Fx 'User=root' "$CONTROLLER_UNIT" >/dev/null
grep -Fx 'Group=root' "$CONTROLLER_UNIT" >/dev/null
grep -Fx 'EnvironmentFile=/etc/hashtree/pool-migration-controller-%i.env' "$CONTROLLER_UNIT" >/dev/null
grep -F 'ExecStart=/usr/local/bin/htree-pool-migration-controller $HTREE_POOL_CONTROLLER_ARGS' "$CONTROLLER_UNIT" >/dev/null
grep -Fx 'Restart=no' "$CONTROLLER_UNIT" >/dev/null
grep -Fx 'TimeoutStartSec=infinity' "$CONTROLLER_UNIT" >/dev/null
grep -Fx 'KillMode=control-group' "$CONTROLLER_UNIT" >/dev/null
grep -Fx 'PrivateNetwork=true' "$CONTROLLER_UNIT" >/dev/null
grep -Fx 'UnsetEnvironment=LD_PRELOAD LD_AUDIT LD_LIBRARY_PATH DYLD_INSERT_LIBRARIES DYLD_LIBRARY_PATH HTREE_LMDB_NO_SYNC HTREE_LMDB_NO_META_SYNC' "$CONTROLLER_UNIT" >/dev/null
grep -Fx 'Type=oneshot' "$UNIT" >/dev/null
grep -Fx 'User=hashtree' "$UNIT" >/dev/null
grep -Fx 'Group=hashtree' "$UNIT" >/dev/null
grep -Fx 'EnvironmentFile=/etc/hashtree/pool-migration-worker-%i.env' "$UNIT" >/dev/null
grep -Fx 'BindsTo=hashtree-pool-migration-controller@%i.service' "$UNIT" >/dev/null
grep -Fx 'After=hashtree-pool-migration-controller@%i.service' "$UNIT" >/dev/null
grep -F 'storage pool migrate-lmdb' "$UNIT" >/dev/null
grep -F -- '--launch-request ${HTREE_POOL_LAUNCH_REQUEST}' "$UNIT" >/dev/null
grep -F -- '--launch-request-wait-seconds ${HTREE_POOL_LAUNCH_WAIT_SECONDS}' "$UNIT" >/dev/null
grep -F -- '$HTREE_POOL_SOURCE_EXTERNAL_ARGS' "$UNIT" >/dev/null
grep -F -- '$HTREE_POOL_LIMIT_ARGS' "$UNIT" >/dev/null
grep -F -- '--max-buffer-mib ${HTREE_POOL_MAX_BUFFER_MIB}' "$UNIT" >/dev/null
grep -F -- '--source-read-concurrency ${HTREE_POOL_SOURCE_READ_CONCURRENCY}' "$UNIT" >/dev/null
grep -F -- '--reopen-batches ${HTREE_POOL_REOPEN_BATCHES}' "$UNIT" >/dev/null
grep -F -- '--resume' "$UNIT" >/dev/null
grep -Fx 'Restart=no' "$UNIT" >/dev/null
grep -Fx 'TimeoutStartSec=infinity' "$UNIT" >/dev/null
grep -Fx 'IOSchedulingClass=idle' "$UNIT" >/dev/null
grep -Fx 'PrivateNetwork=true' "$UNIT" >/dev/null
grep -Fx 'UnsetEnvironment=LD_PRELOAD LD_AUDIT LD_LIBRARY_PATH DYLD_INSERT_LIBRARIES DYLD_LIBRARY_PATH HTREE_LMDB_NO_SYNC HTREE_LMDB_NO_META_SYNC' "$UNIT" >/dev/null
if grep -Eq '^Exec(Condition|StartPre|StartPost|Reload|Stop|StopPost)=' "$UNIT"; then
  echo "migration template must not add unacknowledged auxiliary processes" >&2
  exit 1
fi
if command -v systemd-analyze >/dev/null 2>&1; then
  verify_dir="$(mktemp -d "${TMPDIR:-/tmp}/hashtree-systemd-verify.XXXXXX")"
  trap 'rm -rf "$verify_dir"' EXIT
  verify_unit="${verify_dir}/hashtree-pool-migration-worker@.service"
  sed 's#/usr/local/bin/htree-pool-migration#/bin/true#' "$UNIT" >"$verify_unit"
  verify_controller="${verify_dir}/hashtree-pool-migration-controller@.service"
  sed 's#/usr/local/bin/htree-pool-migration-controller#/bin/true#' \
    "$CONTROLLER_UNIT" >"$verify_controller"
  systemd-analyze verify "$verify_controller" "$verify_unit"
elif [[ "$(uname -s)" == "Linux" ]]; then
  echo "systemd-analyze is required to verify the migration template" >&2
  exit 1
fi
grep -F 'hashtree-pool-migration-worker@.service' "$README" >/dev/null
grep -F 'hashtree-pool-migration-controller@.service' "$README" >/dev/null
grep -F '/usr/local/bin/htree-pool-migration-controller' "$README" >/dev/null
grep -F '/etc/hashtree/pool-migration-controller-NAME.env' "$README" >/dev/null
grep -F 'HTREE_POOL_BATCH_SIZE=4096' "$README" >/dev/null
grep -F 'final-stopped-source' "$README" >/dev/null
grep -F 'final-stopped-full' "$README" >/dev/null
grep -F 'source-terminal.json' "$README" >/dev/null
grep -F 'online-bounded` is deliberately refused' "$README" >/dev/null
grep -F 'systemctl start --no-block hashtree-pool-migration-controller@NAME.service' "$README" >/dev/null
if grep -F 'systemctl start --no-block hashtree-pool-migration-worker@NAME.service' "$README" >/dev/null; then
  echo "runbook must never launch the bound worker directly" >&2
  exit 1
fi
grep -F 'attempts-v3' "$README" >/dev/null
grep -F 'launch-ack.json' "$README" >/dev/null
grep -F 'delete an attempt, request, acknowledgement' "$README" >/dev/null
grep -F 'checkpointSystemctlSubprocesses == authorizedCheckpoints' "$README" >/dev/null
grep -F 'p99 request-to-authorization latency at most 100 ms' "$README" >/dev/null
grep -F 'Do not use a generated fixture for this gate.' "$README" >/dev/null
grep -F 'Id=hashtree-pool-migrate@.service' "$README" >/dev/null
SOURCE_ROOT="$(cd "${SYSTEMD_DIR}/../.." && pwd)"
grep -F '"checkpointSystemctlSubprocesses"' \
  "${SOURCE_ROOT}/rust/crates/hashtree-cli/src/app/pool_migration_controller.rs" >/dev/null
if grep -R -F 'authorize_checkpoint("migration-page-scan"' \
    "${SOURCE_ROOT}/rust/crates/hashtree-cli/src/app" >/dev/null; then
  echo "read-only full page scans must not create root checkpoints" >&2
  exit 1
fi
if grep -R -F 'authorize_checkpoint("source-audit-batch"' \
    "${SOURCE_ROOT}/rust/crates/hashtree-cli/src/app" >/dev/null; then
  echo "read-only source audit batches must not create root checkpoints" >&2
  exit 1
fi

echo "systemd pool migration service template ok"
