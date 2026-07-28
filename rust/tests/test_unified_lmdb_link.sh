#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
rust_dir="$(cd "${script_dir}/.." && pwd)"

assert_one_lmdb_implementation() {
    local tree_file=$1
    local scope=$2
    local sys_packages
    local heed_packages
    local social_graph_packages

    sys_packages="$(
        awk '$1 ~ /lmdb.*-sys$/ { print $1 " " $2 }' "$tree_file" |
            sort -u
    )"
    heed_packages="$(
        awk '$1 == "heed" || $1 == "hashtree-heed" { print $1 " " $2 }' "$tree_file" |
            sort -u
    )"
    social_graph_packages="$(
        awk '$1 == "nostr-social-graph-heed" || $1 == "hashtree-nostr-social-graph-heed" {
            print $1 " " $2
        }' "$tree_file" |
            sort -u
    )"

    if [[ "$sys_packages" != "hashtree-lmdb-master-sys v0.2.6-hashtree.1" ]]; then
        echo "${scope} must resolve exactly one hardened native LMDB package; found:" >&2
        printf '%s\n' "${sys_packages:-<none>}" >&2
        return 1
    fi
    if [[ "$heed_packages" != "hashtree-heed v0.20.5-hashtree.1" ]]; then
        echo "${scope} must resolve exactly one heed wrapper; found:" >&2
        printf '%s\n' "${heed_packages:-<none>}" >&2
        return 1
    fi
    if [[ "$social_graph_packages" != \
        "hashtree-nostr-social-graph-heed v0.1.3-hashtree.1" ]]; then
        echo "${scope} resolved an unexpected social-graph LMDB adapter:" >&2
        printf '%s\n' "${social_graph_packages:-<none>}" >&2
        return 1
    fi
}

cd "$rust_dir"

workspace_tree="$(mktemp "${TMPDIR:-/tmp}/hashtree-workspace-lmdb-tree.XXXXXX")"
downstream_dir="$(mktemp -d "${TMPDIR:-/tmp}/hashtree-downstream-lmdb.XXXXXX")"
downstream_dir="$(cd "$downstream_dir" && pwd -P)"
cleanup() {
    rm -f -- "$workspace_tree"
    rm -rf -- "$downstream_dir"
}
trap cleanup EXIT

cargo tree --workspace --locked --prefix none >"$workspace_tree"
assert_one_lmdb_implementation "$workspace_tree" "workspace"

# Package the unpublished dependency set together, then link a consumer only
# against the extracted `.crate` contents. The local patches stand in for the
# same ordered crates.io releases while retaining Cargo's normalized publish
# manifests and package file selection.
package_target="$downstream_dir/package-target"
CARGO_TARGET_DIR="$package_target" cargo package \
    --locked \
    --allow-dirty \
    --no-verify \
    -p hashtree-lmdb-master-sys \
    -p hashtree-heed \
    -p hashtree-nostr-social-graph-heed \
    -p hashtree-core \
    -p hashtree-lmdb \
    -p git-remote-htree

package_dir="$downstream_dir/packages"
mkdir -p "$package_dir"
for archive in \
    hashtree-lmdb-master-sys-0.2.6-hashtree.1.crate \
    hashtree-heed-0.20.5-hashtree.1.crate \
    hashtree-nostr-social-graph-heed-0.1.3-hashtree.1.crate \
    hashtree-core-0.2.88.crate \
    hashtree-lmdb-0.2.87.crate \
    git-remote-htree-0.2.83.crate
do
    test -f "$package_target/package/$archive"
    tar -xzf "$package_target/package/$archive" -C "$package_dir"
done

mkdir -p "$downstream_dir/src"
cat >"$downstream_dir/Cargo.toml" <<EOF
[package]
name = "hashtree-unified-lmdb-downstream"
version = "0.0.0"
edition = "2021"
publish = false

[dependencies]
hashtree-lmdb = { path = "${package_dir}/hashtree-lmdb-0.2.87" }
heed = { package = "hashtree-heed", path = "${package_dir}/hashtree-heed-0.20.5-hashtree.1" }
nostr-social-graph-heed = { package = "hashtree-nostr-social-graph-heed", path = "${package_dir}/hashtree-nostr-social-graph-heed-0.1.3-hashtree.1" }
git-remote-htree = { path = "${package_dir}/git-remote-htree-0.2.83" }

[patch.crates-io]
hashtree-core = { path = "${package_dir}/hashtree-core-0.2.88" }
hashtree-heed = { path = "${package_dir}/hashtree-heed-0.20.5-hashtree.1" }
hashtree-lmdb = { path = "${package_dir}/hashtree-lmdb-0.2.87" }
hashtree-lmdb-master-sys = { path = "${package_dir}/hashtree-lmdb-master-sys-0.2.6-hashtree.1" }
EOF
cat >"$downstream_dir/src/main.rs" <<'EOF'
use hashtree_lmdb::LmdbBlobStore;
use nostr_social_graph_heed::HeedSocialGraph;

const ROOT: &str = "0000000000000000000000000000000000000000000000000000000000000000";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::args().nth(1).ok_or("missing runtime directory")?;

    let direct_path = std::path::Path::new(&root).join("direct-heed");
    std::fs::create_dir_all(&direct_path)?;
    let mut options = heed::EnvOpenOptions::new();
    options.map_size(16 * 1024 * 1024).max_dbs(1);
    let direct_env = unsafe { options.open(&direct_path)? };
    drop(direct_env);

    let graph = HeedSocialGraph::open(
        std::path::Path::new(&root).join("social-graph"),
        ROOT,
    )?;
    drop(graph);

    let store = LmdbBlobStore::new(std::path::Path::new(&root).join("blobs"))?;
    drop(store);
    let _generated_key = git_remote_htree::generate_secret_key();
    Ok(())
}
EOF

downstream_tree="$downstream_dir/dependency-tree.txt"
cargo tree --manifest-path "$downstream_dir/Cargo.toml" --prefix none >"$downstream_tree"
assert_one_lmdb_implementation "$downstream_tree" "downstream"
for extracted_package in \
    "$package_dir/hashtree-lmdb-master-sys-0.2.6-hashtree.1" \
    "$package_dir/hashtree-heed-0.20.5-hashtree.1" \
    "$package_dir/hashtree-nostr-social-graph-heed-0.1.3-hashtree.1" \
    "$package_dir/hashtree-core-0.2.88" \
    "$package_dir/hashtree-lmdb-0.2.87" \
    "$package_dir/git-remote-htree-0.2.83"
do
    grep -F "($extracted_package)" "$downstream_tree" >/dev/null
done

downstream_target="${CARGO_TARGET_DIR:-$rust_dir/target}/unified-lmdb-downstream"
cargo build --manifest-path "$downstream_dir/Cargo.toml" \
    --locked \
    --target-dir "$downstream_target"

runtime_dir="$downstream_dir/runtime"
mkdir -p "$runtime_dir"
"$downstream_target/debug/hashtree-unified-lmdb-downstream" "$runtime_dir"

echo "unified LMDB workspace and downstream link checks passed"
