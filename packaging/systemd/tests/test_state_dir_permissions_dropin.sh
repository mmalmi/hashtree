#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SYSTEMD_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
DROPIN="${SYSTEMD_DIR}/hashtree.service.d/state-dir-permissions.conf"
README="${SYSTEMD_DIR}/README.md"

test -f "$DROPIN"
grep -Fx '[Service]' "$DROPIN" >/dev/null
grep -Fx 'ExecStartPre=+/usr/bin/install -d -o hashtree -g hashtree -m 0755 /srv/hashtree/state' "$DROPIN" >/dev/null
grep -F 'ExecStartPre=+' "$README" >/dev/null
grep -F '/srv/hashtree/state' "$README" >/dev/null

echo "systemd state-dir permissions drop-in ok"
