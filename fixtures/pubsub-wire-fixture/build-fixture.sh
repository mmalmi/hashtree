#!/bin/sh
set -eu
cargo metadata --locked --offline --filter-platform x86_64-unknown-linux-gnu --format-version=1 > /stage/evidence/cargo-metadata.json
cargo tree --locked --offline --target x86_64-unknown-linux-gnu --package nostr-pubsub-fips --edges features > /stage/evidence/cargo-feature-tree.txt
cargo build --locked --offline --package nostr-pubsub-fips --example fips-pubsub-wire-stdio --message-format=json > /stage/evidence/cargo-build.jsonl 2> /stage/evidence/cargo-build.stderr
