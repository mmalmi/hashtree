#!/bin/bash
# Publish all hashtree crates to crates.io in dependency order
#
# Usage:
#   ./scripts/publish.sh        # Publish all crates
#   ./scripts/publish.sh --dry-run  # Test without publishing

set -e

DRY_RUN=""
ALLOW_DIRTY="--allow-dirty"

if [[ "$1" == "--dry-run" ]]; then
    DRY_RUN="--dry-run"
    echo "=== DRY RUN MODE ==="
fi

# Wait time between publishes for crates.io indexing (seconds)
WAIT_TIME=30

publish_crate() {
    local crate=$1
    local extra_flags=${2:-""}

    echo ""
    echo "=========================================="
    echo "Publishing: $crate"
    echo "=========================================="

    if cargo publish -p "$crate" $DRY_RUN $ALLOW_DIRTY $extra_flags; then
        echo "✓ $crate published successfully"

        if [[ -z "$DRY_RUN" ]]; then
            echo "Waiting ${WAIT_TIME}s for crates.io to index..."
            sleep $WAIT_TIME
        fi
    else
        echo "✗ Failed to publish $crate (continuing...)"
    fi
}

echo "Publishing hashtree crates to crates.io"
echo ""

# Check if logged in
if [[ -z "$DRY_RUN" ]]; then
    echo "Checking crates.io authentication..."
    if ! cargo login --help > /dev/null 2>&1; then
        echo "Please run 'cargo login' first"
        exit 1
    fi
fi

# Tier 1: No internal dependencies
publish_crate "hashtree-core"
publish_crate "hashtree-config"
# hashtree-bep52 excluded - internal testing only

# Tier 2: Depends on hashtree-core only
publish_crate "hashtree-lmdb"
publish_crate "hashtree-fs"
publish_crate "hashtree-s3"
publish_crate "hashtree-blossom"  # optional deps on core, config
publish_crate "hashtree-resolver"
# hashtree-sim excluded - internal testing only

# Tier 3: Depends on hashtree-core only (hashtree-sim is dev-dependency)
publish_crate "hashtree-webrtc"

# Tier 5: Depends on multiple crates
publish_crate "git-remote-htree"
publish_crate "hashtree-cli"  # depends on git-remote-htree

echo ""
echo "=========================================="
echo "✓ All crates published successfully!"
echo "=========================================="
