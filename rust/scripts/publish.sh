#!/bin/bash
# Publish all hashtree crates to crates.io in dependency stages
#
# Usage:
#   ./scripts/publish.sh             # Publish all crates
#   ./scripts/publish.sh --dry-run   # Test without publishing
#   ./scripts/publish.sh --plan      # Print publish order

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

# Wait time between dependency stages for crates.io indexing (seconds)
WAIT_TIME=30

FAILED_CRATES=()

STAGE_1_CRATES=(
    "hashtree-core"
    "hashtree-config"
    "hashtree-merge"
)

STAGE_2_CRATES=(
    "hashtree-index"
    "hashtree-lmdb"
    "hashtree-fuse"
    "hashtree-s3"
    "hashtree-blossom"
    "hashtree-resolver"
)

STAGE_3_CRATES=(
    "hashtree-fs"
    "hashtree-collection"
    "hashtree-ffi"
    "hashtree-network"
    "hashtree-updater"
)

STAGE_4_CRATES=(
    "hashtree-nostr"
    "hashtree-fips-transport"
)

STAGE_5_CRATES=(
    "hashtree-nostr-pubsub"
    "git-remote-htree"
    "tauri-plugin-hashtree-updater"
)

STAGE_6_CRATES=(
    "hashtree-cli"
)

STAGE_7_CRATES=(
    "hashtree-cashu-cli"
    "hashtree-embedded"
)

STAGE_8_CRATES=(
    "hashtree-embedded-ffi"
)

ALL_CRATES=(
    "${STAGE_1_CRATES[@]}"
    "${STAGE_2_CRATES[@]}"
    "${STAGE_3_CRATES[@]}"
    "${STAGE_4_CRATES[@]}"
    "${STAGE_5_CRATES[@]}"
    "${STAGE_6_CRATES[@]}"
    "${STAGE_7_CRATES[@]}"
    "${STAGE_8_CRATES[@]}"
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
    elif echo "$output" | grep -q "already exists"; then
        echo "✓ $crate already published at this version (skipping)"
    else
        echo "$output"
        echo "✗ Failed to publish $crate (continuing...)"
        return 1
    fi

    return 0
}

publish_stage() {
    local stage_name=$1
    shift

    local crates=("$@")
    local log_dir
    log_dir=$(mktemp -d "${TMPDIR:-/tmp}/hashtree-publish.XXXXXX")
    local pids=()
    local crate

    echo ""
    echo "=== ${stage_name}: ${crates[*]} ==="

    for crate in "${crates[@]}"; do
        publish_crate "$crate" >"${log_dir}/${crate}.log" 2>&1 &
        pids+=("$!")
    done

    local published=0
    local status=0
    local i
    for i in "${!pids[@]}"; do
        crate="${crates[$i]}"
        if ! wait "${pids[$i]}"; then
            FAILED_CRATES+=("$crate")
            status=1
        fi

        cat "${log_dir}/${crate}.log"
        if grep -q "published successfully" "${log_dir}/${crate}.log"; then
            published=1
        fi
    done

    rm -rf "$log_dir"

    if [[ "$status" -eq 0 && "$published" -eq 1 && -z "$DRY_RUN" ]]; then
        echo ""
        echo "Waiting ${WAIT_TIME}s for crates.io to index this stage..."
        sleep "$WAIT_TIME"
    fi

    return 0
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

publish_stage "Stage 1" "${STAGE_1_CRATES[@]}"
# hashtree-bep52 excluded - internal testing only

publish_stage "Stage 2" "${STAGE_2_CRATES[@]}"
# hashtree-sim excluded - internal testing only

publish_stage "Stage 3" "${STAGE_3_CRATES[@]}"
publish_stage "Stage 4" "${STAGE_4_CRATES[@]}"
publish_stage "Stage 5" "${STAGE_5_CRATES[@]}"
publish_stage "Stage 6" "${STAGE_6_CRATES[@]}"
publish_stage "Stage 7" "${STAGE_7_CRATES[@]}"
publish_stage "Stage 8" "${STAGE_8_CRATES[@]}"

echo ""
echo "=========================================="
if [[ ${#FAILED_CRATES[@]} -eq 0 ]]; then
    echo "✓ All crates published successfully!"
else
    echo "✗ Failed to publish: ${FAILED_CRATES[*]}"
    exit 1
fi
echo "=========================================="
