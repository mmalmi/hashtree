#!/bin/bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RELEASE_TO_HTREE="${ROOT_DIR}/rust/scripts/release_to_htree.sh"
PUBLISH_RELEASE="${ROOT_DIR}/rust/scripts/publish_release.sh"
CLI_ARGS="${ROOT_DIR}/rust/crates/hashtree-cli/src/app/args.rs"

# The source manifest and released CLI must describe the same package version.
haps_version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "${ROOT_DIR}/haps.toml")"
cli_version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "${ROOT_DIR}/rust/crates/hashtree-cli/Cargo.toml")"
if [[ -z "$haps_version" || "$haps_version" != "$cli_version" ]]; then
    echo "haps.toml package version must match hashtree-cli (${cli_version})" >&2
    exit 1
fi

grep -F 'TREE_NAME="releases/${REPO_NAME}"' "$RELEASE_TO_HTREE" >/dev/null
grep -F 'tree_name="${3:-releases/${repo_name}}"' "$PUBLISH_RELEASE" >/dev/null
grep -F 'Mutable release tree name (repo releases usually use "releases/<repo>")' "$CLI_ARGS" >/dev/null

echo "release script default tree naming checks passed"
