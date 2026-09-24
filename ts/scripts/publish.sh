#!/bin/bash
# Build and publish npm archives, or inspect them with --plan / --dry-run.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
exec node "${SCRIPT_DIR}/publish-npm.mjs" "$@"
