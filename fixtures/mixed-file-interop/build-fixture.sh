#!/bin/sh
set -eu
cargo metadata --locked --offline --filter-platform x86_64-unknown-linux-gnu --format-version=1 > /stage/evidence/cargo-metadata.json
cargo tree --locked --offline --target x86_64-unknown-linux-gnu --package runtime61-mixed-file-fixture --edges features > /stage/evidence/cargo-feature-tree.txt
cargo build --locked --offline --package runtime61-mixed-file-fixture --message-format=json > /stage/evidence/cargo-build.jsonl 2> /stage/evidence/cargo-build.stderr
