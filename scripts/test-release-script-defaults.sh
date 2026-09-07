#!/bin/bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RELEASE_TO_HTREE="${ROOT_DIR}/rust/scripts/release_to_htree.sh"
PUBLISH_RELEASE="${ROOT_DIR}/rust/scripts/publish_release.sh"
CLI_ARGS="${ROOT_DIR}/rust/crates/hashtree-cli/src/app/args.rs"

grep -F 'TREE_NAME="releases/${REPO_NAME}"' "$RELEASE_TO_HTREE" >/dev/null
grep -F 'tree_name="${3:-releases/${repo_name}}"' "$PUBLISH_RELEASE" >/dev/null
grep -F 'Mutable release tree name (repo releases usually use "releases/<repo>")' "$CLI_ARGS" >/dev/null

echo "release script default tree naming checks passed"
