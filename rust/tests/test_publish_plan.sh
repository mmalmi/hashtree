#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUST_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
PUBLISH_SCRIPT="${RUST_DIR}/scripts/publish.sh"

PLAN_OUTPUT="$("${PUBLISH_SCRIPT}" --plan)"

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

require_line() {
    grep -Fx "$2" "$1" >/dev/null || {
        echo "$3" >&2
        exit 1
    }
}

require_line "${RUST_DIR}/Cargo.toml" 'version = "0.2.82"' \
    "unchanged workspace crates must remain at 0.2.82"
for crate in hashtree-core hashtree-cli hashtree-embedded; do
    require_line "${RUST_DIR}/crates/${crate}/Cargo.toml" 'version = "0.2.83"' \
        "${crate} must release as 0.2.83"
done
require_line "${RUST_DIR}/crates/hashtree-fips-transport/Cargo.toml" 'version = "0.3.0"' \
    "hashtree-fips-transport must remain at 0.3.0"

grep -F 'hashtree-core = { version = "0.2.83", path = "crates/hashtree-core" }' \
    "${RUST_DIR}/Cargo.toml" >/dev/null
grep -F 'hashtree-fips-transport = { version = "0.3.0", path = "crates/hashtree-fips-transport" }' \
    "${RUST_DIR}/Cargo.toml" >/dev/null
grep -F 'hashtree-cli = { version = "0.2.83", path = "../hashtree-cli", default-features = false, features = ["lmdb"] }' \
    "${RUST_DIR}/crates/hashtree-embedded/Cargo.toml" >/dev/null

require_registry_lock() {
    local package="$1" version="$2" checksum="$3" lock
    lock="$(awk -v package="$package" \
        '/^\[\[package\]\]$/ { capture = 0 } $0 == "name = \"" package "\"" { capture = 1 } capture' \
        "${RUST_DIR}/Cargo.lock")"
    printf '%s\n' "$lock" | grep -Fx "version = \"${version}\"" >/dev/null
    printf '%s\n' "$lock" | grep -Fx 'source = "registry+https://github.com/rust-lang/crates.io-index"' >/dev/null
    printf '%s\n' "$lock" | grep -Fx "checksum = \"${checksum}\"" >/dev/null
}

require_registry_lock fips-core 0.4.0 5eb5c2cd49701461cfe2a9604eec3ddad6d3fadca67aceb11f472b6e665ecf89
require_registry_lock fips-identity 0.3.1 e143619aebf9db3129c1d2de67ba223bcf611216efa09a932c98a617e3e4a42b
require_registry_lock fips-tcp 0.2.0 d18861c5eca7c472fbbdbbfb498f8d2525405081a9a24b42633c600ba6f6e42a
require_registry_lock fips-tcp-endpoint 0.2.0 8e3e01e352b709b80f4261e2cd7d0ffde2d3aaf175267b3960997e70f7305c12
require_registry_lock nostr-pubsub 0.1.10 0a3c668aede5ebf20501199206e1853ee8e26f38d476a60f54fb27443dfb552f
require_registry_lock nostr-pubsub-fips 0.3.0 c2e2904004e5d0e55a676db596f8f052e171eabd236799b5aec7718b04a0a79e
require_registry_lock nostr-pubsub-social-graph 0.2.2 9e1d9357bc482537beaf82ca020202063cdd62391d011341fb723869e0f22550

if printf '%s\n' "$PLAN_OUTPUT" | grep -Fx 'cashu-service' >/dev/null; then
    echo "cashu-service is published from the standalone cashu-service repo, not hashtree" >&2
    exit 1
fi

printf '%s\n' "$PLAN_OUTPUT" | grep -Fx 'hashtree-fuse' >/dev/null
printf '%s\n' "$PLAN_OUTPUT" | grep -Fx 'hashtree-collection' >/dev/null
printf '%s\n' "$PLAN_OUTPUT" | grep -Fx 'hashtree-cashu-cli' >/dev/null

plan_line() {
    printf '%s\n' "$PLAN_OUTPUT" | nl -ba | awk -v crate="$1" '$2 == crate { print $1; exit }'
}

fuse_line="$(plan_line hashtree-fuse)"
core_line="$(plan_line hashtree-core)"
collection_line="$(plan_line hashtree-collection)"
nostr_line="$(plan_line hashtree-nostr)"
transport_line="$(plan_line hashtree-fips-transport)"
cli_line="$(plan_line hashtree-cli)"
cashu_cli_line="$(plan_line hashtree-cashu-cli)"
embedded_line="$(plan_line hashtree-embedded)"

if [ -z "$fuse_line" ] || [ -z "$core_line" ] || [ -z "$collection_line" ] || \
    [ -z "$nostr_line" ] || [ -z "$transport_line" ] || [ -z "$cli_line" ] || \
    [ -z "$cashu_cli_line" ] || [ -z "$embedded_line" ]; then
    echo "Failed to find a required crate in the publish plan" >&2
    exit 1
fi

if [ "$core_line" -ge "$transport_line" ] || [ "$transport_line" -ge "$cli_line" ] || \
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
