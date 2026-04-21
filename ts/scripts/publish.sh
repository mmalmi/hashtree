#!/bin/bash
# Publish hashtree npm packages in dependency order.
#
# Usage:
#   ./scripts/publish.sh
#   ./scripts/publish.sh --dry-run
#   ./scripts/publish.sh --plan

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TS_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"

PLAN_ONLY=0
DRY_RUN=0

for arg in "$@"; do
    case "$arg" in
        --plan)
            PLAN_ONLY=1
            ;;
        --dry-run)
            DRY_RUN=1
            ;;
        *)
            echo "Unknown argument: $arg" >&2
            exit 1
            ;;
    esac
done

PACKAGES=(
    "@hashtree/core"
    "@hashtree/merge"
    "@hashtree/dexie"
    "@hashtree/git"
    "@hashtree/index"
    "@hashtree/collection"
    "@hashtree/mesh"
    "@hashtree/nostr"
    "@hashtree/worker"
)

if [[ "$PLAN_ONLY" -eq 1 ]]; then
    for pkg in "${PACKAGES[@]}"; do
        echo "$pkg"
    done
    exit 0
fi

cd "$TS_DIR"

if ! command -v pnpm >/dev/null 2>&1; then
    echo "pnpm is required" >&2
    exit 1
fi

if [[ "$DRY_RUN" -eq 1 ]]; then
    echo "=== DRY RUN MODE ==="
else
    echo "Checking npm authentication..."
    if ! npm whoami >/dev/null 2>&1; then
        echo "Please run 'npm login' first" >&2
        exit 1
    fi
fi

FAILED_PACKAGES=()

publish_package() {
    local pkg="$1"

    echo
    echo "=========================================="
    echo "Publishing: $pkg"
    echo "=========================================="

    pnpm --filter "$pkg" build

    local publish_flags=(--no-git-checks)
    if [[ "$DRY_RUN" -eq 1 ]]; then
        publish_flags+=(--dry-run)
    fi

    local output
    if output=$(pnpm --filter "$pkg" publish "${publish_flags[@]}" 2>&1); then
        echo "$output"
        echo "✓ $pkg published successfully"
    elif echo "$output" | grep -qi "previously published versions"; then
        echo "$output"
        echo "✓ $pkg already published at this version (skipping)"
    else
        echo "$output"
        echo "✗ Failed to publish $pkg (continuing...)"
        FAILED_PACKAGES+=("$pkg")
    fi
}

echo "Publishing hashtree npm packages"

for pkg in "${PACKAGES[@]}"; do
    publish_package "$pkg"
done

echo
echo "=========================================="
if [[ ${#FAILED_PACKAGES[@]} -eq 0 ]]; then
    echo "✓ All packages published successfully!"
else
    echo "✗ Failed to publish: ${FAILED_PACKAGES[*]}"
    exit 1
fi
echo "=========================================="
