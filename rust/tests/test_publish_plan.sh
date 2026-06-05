#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUST_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
PUBLISH_SCRIPT="${RUST_DIR}/scripts/publish.sh"

PLAN_OUTPUT="$("${PUBLISH_SCRIPT}" --plan)"

if printf '%s\n' "$PLAN_OUTPUT" | grep -Fx 'cashu-service' >/dev/null; then
    echo "cashu-service is published from the standalone cashu-service repo, not hashtree" >&2
    exit 1
fi

printf '%s\n' "$PLAN_OUTPUT" | grep -Fx 'hashtree-fuse' >/dev/null
printf '%s\n' "$PLAN_OUTPUT" | grep -Fx 'hashtree-collection' >/dev/null
printf '%s\n' "$PLAN_OUTPUT" | grep -Fx 'hashtree-cashu-cli' >/dev/null

fuse_line="$(printf '%s\n' "$PLAN_OUTPUT" | nl -ba | awk '$2 == "hashtree-fuse" { print $1; exit }')"
collection_line="$(printf '%s\n' "$PLAN_OUTPUT" | nl -ba | awk '$2 == "hashtree-collection" { print $1; exit }')"
nostr_line="$(printf '%s\n' "$PLAN_OUTPUT" | nl -ba | awk '$2 == "hashtree-nostr" { print $1; exit }')"
cli_line="$(printf '%s\n' "$PLAN_OUTPUT" | nl -ba | awk '$2 == "hashtree-cli" { print $1; exit }')"
cashu_cli_line="$(printf '%s\n' "$PLAN_OUTPUT" | nl -ba | awk '$2 == "hashtree-cashu-cli" { print $1; exit }')"

if [ -z "$fuse_line" ] || [ -z "$collection_line" ] || [ -z "$nostr_line" ] || [ -z "$cli_line" ] || [ -z "$cashu_cli_line" ]; then
    echo "Failed to find hashtree-fuse, hashtree-collection, hashtree-nostr, hashtree-cli, or hashtree-cashu-cli in publish plan" >&2
    exit 1
fi

if [ "$cli_line" -ge "$cashu_cli_line" ]; then
    echo "hashtree-cli must be published before hashtree-cashu-cli" >&2
    exit 1
fi

if [ "$fuse_line" -ge "$cli_line" ]; then
    echo "hashtree-fuse must be published before hashtree-cli" >&2
    exit 1
fi

if [ "$collection_line" -ge "$nostr_line" ]; then
    echo "hashtree-collection must be published before hashtree-nostr" >&2
    exit 1
fi

echo "test_publish_plan.sh passed"
