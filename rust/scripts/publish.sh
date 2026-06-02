#!/bin/bash
# Publish all hashtree crates to crates.io in dependency order
#
# Usage:
#   ./scripts/publish.sh        # Publish all crates
#   ./scripts/publish.sh --dry-run  # Test without publishing
#   ./scripts/publish.sh --plan     # Print publish order

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUST_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

DRY_RUN=""
PLAN_ONLY=0
ALLOW_DIRTY="--allow-dirty"

for arg in "$@"; do
    case "$arg" in
        --dry-run)
            DRY_RUN="--dry-run"
            ;;
        --plan)
            PLAN_ONLY=1
            ;;
        *)
            echo "Unknown argument: $arg" >&2
            exit 1
            ;;
    esac
done

# Wait time between publishes for crates.io indexing (seconds)
WAIT_TIME=30

FAILED_CRATES=()

TIER_1_CRATES=(
    "hashtree-core"
    "hashtree-config"
    "hashtree-merge"
    "cashu-service"
)

TIER_2_CRATES=(
    "hashtree-index"
    "hashtree-lmdb"
    "hashtree-fs"
    "hashtree-fuse"
    "hashtree-s3"
    "hashtree-blossom"
    "hashtree-resolver"
)

TIER_3_CRATES=(
    "hashtree-collection"
    "hashtree-nostr"
    "hashtree-network"
    "hashtree-fips-transport"
    "hashtree-updater"
)

TIER_4_CRATES=(
    "git-remote-htree"
    "tauri-plugin-hashtree-updater"
    "hashtree-cli"
    "hashtree-cashu-cli"
)

ALL_CRATES=(
    "${TIER_1_CRATES[@]}"
    "${TIER_2_CRATES[@]}"
    "${TIER_3_CRATES[@]}"
    "${TIER_4_CRATES[@]}"
)

publish_crate() {
    local crate=$1
    local extra_flags=${2:-""}

    echo ""
    echo "=========================================="
    echo "Publishing: $crate"
    echo "=========================================="

    local output
    if output=$(cargo publish -p "$crate" $DRY_RUN $ALLOW_DIRTY $extra_flags 2>&1); then
        echo "$output"
        echo "✓ $crate published successfully"

        if [[ -z "$DRY_RUN" ]]; then
            echo "Waiting ${WAIT_TIME}s for crates.io to index..."
            sleep $WAIT_TIME
        fi
    elif echo "$output" | grep -q "already exists"; then
        echo "✓ $crate already published at this version (skipping)"
    else
        echo "$output"
        echo "✗ Failed to publish $crate (continuing...)"
        FAILED_CRATES+=("$crate")
    fi
}

if [[ "$PLAN_ONLY" -eq 1 ]]; then
    printf '%s\n' "${ALL_CRATES[@]}"
    exit 0
fi

if [[ -n "$DRY_RUN" ]]; then
    echo "=== DRY RUN MODE ==="
fi

echo "Publishing hashtree crates to crates.io"
echo ""

cd "$RUST_DIR"

# Tier 1: No internal dependencies
for crate in "${TIER_1_CRATES[@]}"; do
    publish_crate "$crate"
done
# hashtree-bep52 excluded - internal testing only

# Tier 2: Depends on hashtree-core only
for crate in "${TIER_2_CRATES[@]}"; do
    publish_crate "$crate"
done
# hashtree-sim excluded - internal testing only

# Tier 3: Depends on published core/index crates
for crate in "${TIER_3_CRATES[@]}"; do
    publish_crate "$crate"
done

# Tier 4: Depends on multiple crates
for crate in "${TIER_4_CRATES[@]}"; do
    publish_crate "$crate"
done

echo ""
echo "=========================================="
if [[ ${#FAILED_CRATES[@]} -eq 0 ]]; then
    echo "✓ All crates published successfully!"
else
    echo "✗ Failed to publish: ${FAILED_CRATES[*]}"
    exit 1
fi
echo "=========================================="
