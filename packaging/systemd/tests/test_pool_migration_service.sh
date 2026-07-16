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
grep -F -- '--resume' "$UNIT" >/dev/null
grep -Fx 'Restart=on-failure' "$UNIT" >/dev/null
grep -Fx 'TimeoutStartSec=infinity' "$UNIT" >/dev/null
grep -Fx 'IOSchedulingClass=idle' "$UNIT" >/dev/null
grep -F 'hashtree-pool-migrate@.service' "$README" >/dev/null
grep -F 'final stopped-write pass' "$README" >/dev/null

echo "systemd pool migration service template ok"
