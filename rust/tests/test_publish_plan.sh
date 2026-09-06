#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUST_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
PUBLISH_SCRIPT="${RUST_DIR}/scripts/publish.sh"

PLAN_OUTPUT="$("${PUBLISH_SCRIPT}" --plan)"

lmdb_version="$(awk -F '"' '/^version = / { print $2; exit }' "${RUST_DIR}/crates/hashtree-lmdb/Cargo.toml")"
: "${lmdb_version:?missing hashtree-lmdb package version}"
grep -F "hashtree-lmdb = { version = \"=${lmdb_version}\", path = \"crates/hashtree-lmdb\" }" \
    "${RUST_DIR}/Cargo.toml" >/dev/null
git_remote_version="$(awk -F '"' '/^version = / { print $2; exit }' "${RUST_DIR}/crates/git-remote-htree/Cargo.toml")"
: "${git_remote_version:?missing git-remote-htree package version}"
grep -F "git-remote-htree = { version = \"=${git_remote_version}\", path = \"../git-remote-htree\" }" \
    "${RUST_DIR}/crates/hashtree-cli/Cargo.toml" >/dev/null
grep -F 'hashtree-fips-transport = { version = "0.4.13", path = "crates/hashtree-fips-transport" }' \
    "${RUST_DIR}/Cargo.toml" >/dev/null
grep -F 'nostr-pubsub-fips = "0.4.17"' \
    "${RUST_DIR}/Cargo.toml" >/dev/null
grep -F 'version = "0.4.13"' \
    "${RUST_DIR}/crates/hashtree-fips-transport/Cargo.toml" >/dev/null
grep -F 'fips-core = { package = "nvpn-fips-core", version = "=0.4.74" }' \
    "${RUST_DIR}/crates/hashtree-fips-transport/Cargo.toml" >/dev/null
grep -F 'fips-tcp = { package = "nvpn-fips-tcp", version = "=0.2.1" }' \
    "${RUST_DIR}/crates/hashtree-fips-transport/Cargo.toml" >/dev/null
grep -F 'fips-tcp-endpoint = { package = "nvpn-fips-tcp-endpoint", version = "=0.2.10" }' \
    "${RUST_DIR}/crates/hashtree-fips-transport/Cargo.toml" >/dev/null
grep -F 'hashtree-core = { version = "0.2.86", path = "../hashtree-core" }' \
    "${RUST_DIR}/crates/hashtree-fips-transport/Cargo.toml" >/dev/null

fake_bin="$(mktemp -d "${TMPDIR:-/tmp}/hashtree-publish-test.XXXXXX")"
trap 'rm -rf "${fake_bin}"' EXIT
printf '%s\n' '#!/bin/sh' 'echo "crate version already exists" >&2' 'exit 1' \
    >"${fake_bin}/cargo"
chmod +x "${fake_bin}/cargo"

SKIP_OUTPUT="$(PATH="${fake_bin}:${PATH}" "${PUBLISH_SCRIPT}" --dry-run)"
planned_count="$(printf '%s\n' "$PLAN_OUTPUT" | awk 'NF { count++ } END { print count }')"
skipped_count="$(printf '%s\n' "$SKIP_OUTPUT" | grep -cF 'already published at this version (skipping)')"
if [ "$planned_count" -ne "$skipped_count" ]; then
    echo "unchanged registry versions must skip without failing the release" >&2
    exit 1
fi

if printf '%s\n' "$PLAN_OUTPUT" | grep -Fx 'cashu-service' >/dev/null; then
    echo "cashu-service is published from the standalone cashu-service repo, not hashtree" >&2
    exit 1
fi

printf '%s\n' "$PLAN_OUTPUT" | grep -Fx 'hashtree-fuse' >/dev/null
printf '%s\n' "$PLAN_OUTPUT" | grep -Fx 'hashtree-collection' >/dev/null
printf '%s\n' "$PLAN_OUTPUT" | grep -Fx 'hashtree-cashu-cli' >/dev/null
printf '%s\n' "$PLAN_OUTPUT" | grep -Fx 'hashtree-lmdb-master-sys' >/dev/null
printf '%s\n' "$PLAN_OUTPUT" | grep -Fx 'hashtree-heed' >/dev/null
printf '%s\n' "$PLAN_OUTPUT" | grep -Fx 'hashtree-nostr-social-graph-heed' >/dev/null

plan_line() {
    printf '%s\n' "$PLAN_OUTPUT" | nl -ba | awk -v crate="$1" '$2 == crate { print $1; exit }'
}

fuse_line="$(plan_line hashtree-fuse)"
core_line="$(plan_line hashtree-core)"
collection_line="$(plan_line hashtree-collection)"
nostr_line="$(plan_line hashtree-nostr)"
transport_line="$(plan_line hashtree-fips-transport)"
nostr_pubsub_line="$(plan_line hashtree-nostr-pubsub)"
cli_line="$(plan_line hashtree-cli)"
cashu_cli_line="$(plan_line hashtree-cashu-cli)"
embedded_line="$(plan_line hashtree-embedded)"
git_remote_line="$(plan_line git-remote-htree)"
lmdb_sys_line="$(plan_line hashtree-lmdb-master-sys)"
heed_line="$(plan_line hashtree-heed)"
social_graph_heed_line="$(plan_line hashtree-nostr-social-graph-heed)"
lmdb_line="$(plan_line hashtree-lmdb)"

if [ -z "$fuse_line" ] || [ -z "$core_line" ] || [ -z "$collection_line" ] || \
    [ -z "$nostr_line" ] || [ -z "$transport_line" ] || \
    [ -z "$nostr_pubsub_line" ] || [ -z "$cli_line" ] || \
    [ -z "$cashu_cli_line" ] || [ -z "$embedded_line" ] || \
    [ -z "$git_remote_line" ] || \
    [ -z "$lmdb_sys_line" ] || [ -z "$heed_line" ] || \
    [ -z "$social_graph_heed_line" ] || [ -z "$lmdb_line" ]; then
    echo "Failed to find a required crate in the publish plan" >&2
    exit 1
fi

if [ "$lmdb_sys_line" -ge "$heed_line" ] || \
    [ "$heed_line" -ge "$social_graph_heed_line" ] || \
    [ "$heed_line" -ge "$lmdb_line" ] || \
    [ "$lmdb_line" -ge "$git_remote_line" ] || \
    [ "$git_remote_line" -ge "$cli_line" ] || \
    [ "$social_graph_heed_line" -ge "$cli_line" ]; then
    echo "registry release order must publish the unified hardened LMDB graph before consumers" >&2
    exit 1
fi

if [ "$core_line" -ge "$transport_line" ] || [ "$transport_line" -ge "$cli_line" ] || \
    [ "$nostr_pubsub_line" -ge "$cli_line" ] || \
    [ "$cli_line" -ge "$embedded_line" ]; then
    echo "registry release order must be core -> transport -> CLI -> embedded" >&2
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
