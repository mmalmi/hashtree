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
require_line "${RUST_DIR}/crates/hashtree-core/Cargo.toml" 'version = "0.2.86"' \
    "hashtree-core must release its process-local route context API"
require_line "${RUST_DIR}/crates/hashtree-lmdb/Cargo.toml" 'version = "0.2.85"' \
    "hashtree-lmdb must release its automatic temperature balancer"
require_line "${RUST_DIR}/crates/hashtree-config/Cargo.toml" 'version = "0.2.83"' \
    "hashtree-config must release without retired peer settings"
require_line "${RUST_DIR}/crates/hashtree-network/Cargo.toml" 'version = "0.2.87"' \
    "hashtree-network must release routed reads and mesh forwarding ownership"
require_line "${RUST_DIR}/crates/hashtree-nostr/Cargo.toml" 'version = "0.2.83"' \
    "hashtree-nostr must release optional negentropy timeout fallback"
require_line "${RUST_DIR}/crates/hashtree-cli/Cargo.toml" 'version = "0.2.100"' \
    "hashtree-cli must release as 0.2.100"
require_line "${RUST_DIR}/crates/hashtree-embedded/Cargo.toml" 'version = "0.2.86"' \
    "hashtree-embedded must release the corrected mesh forwarding boundary"
require_line "${RUST_DIR}/crates/hashtree-fips-transport/Cargo.toml" 'version = "0.4.8"' \
    "hashtree-fips-transport must release idle-safe adaptive polling"

grep -F 'hashtree-core = { version = "0.2.86", path = "crates/hashtree-core" }' \
    "${RUST_DIR}/Cargo.toml" >/dev/null
grep -F 'hashtree-lmdb = { version = "0.2.85", path = "crates/hashtree-lmdb" }' \
    "${RUST_DIR}/Cargo.toml" >/dev/null
grep -F 'hashtree-config = { version = "0.2.83", path = "crates/hashtree-config" }' \
    "${RUST_DIR}/Cargo.toml" >/dev/null
grep -F 'hashtree-network = { version = "0.2.87", path = "crates/hashtree-network" }' \
    "${RUST_DIR}/Cargo.toml" >/dev/null
grep -F 'hashtree-nostr = { version = "0.2.83", path = "crates/hashtree-nostr" }' \
    "${RUST_DIR}/Cargo.toml" >/dev/null
grep -F 'hashtree-fips-transport = { version = "0.4.8", path = "crates/hashtree-fips-transport" }' \
    "${RUST_DIR}/Cargo.toml" >/dev/null
grep -F 'hashtree-cli = { version = "0.2.100", path = "../hashtree-cli", default-features = false, features = ["lmdb", "fips-webrtc"] }' \
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

require_registry_lock fips-core 0.4.6 12cc0df5e04a1aae16efa85313976e87eb037d6e7955b8a035febd91b00383dc
require_registry_lock fips-identity 0.3.1 e143619aebf9db3129c1d2de67ba223bcf611216efa09a932c98a617e3e4a42b
require_registry_lock fips-tcp 0.2.0 d18861c5eca7c472fbbdbbfb498f8d2525405081a9a24b42633c600ba6f6e42a
require_registry_lock fips-tcp-endpoint 0.2.0 8e3e01e352b709b80f4261e2cd7d0ffde2d3aaf175267b3960997e70f7305c12
require_registry_lock nostr-pubsub 0.1.11 f3c509d68c8de0f87de781630cca6d3d4e61b2fe8a1ba3d21463efd7bc4780c6
require_registry_lock nostr-pubsub-fips 0.3.1 5663a6108ae432879d6d7441036b979605fc032011c0a6e81dbf1798ce844f6c
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
