#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SYSTEMD_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
UNIT="${SYSTEMD_DIR}/hashtree-pool-migrate@.service"
README="${SYSTEMD_DIR}/README.md"

test -f "$UNIT"
grep -Fx 'Type=oneshot' "$UNIT" >/dev/null
grep -Fx 'EnvironmentFile=/etc/hashtree/pool-migrate-%i.env' "$UNIT" >/dev/null
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
  verify_unit="${verify_dir}/hashtree-pool-migrate@.service"
  sed 's#/usr/local/bin/htree-pool-migration#/bin/true#' "$UNIT" >"$verify_unit"
  systemd-analyze verify "$verify_unit"
elif [[ "$(uname -s)" == "Linux" ]]; then
  echo "systemd-analyze is required to verify the migration template" >&2
  exit 1
fi
grep -F 'hashtree-pool-migrate@.service' "$README" >/dev/null
grep -F 'final-stopped-full' "$README" >/dev/null
grep -F 'attempts-v3' "$README" >/dev/null
grep -F 'launch-ack.json' "$README" >/dev/null

echo "systemd pool migration service template ok"
