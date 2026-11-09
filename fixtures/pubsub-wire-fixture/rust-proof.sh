#!/bin/sh
set -eu
rustc -Vv > /stage/evidence/rustc-version.txt
cargo -V > /stage/evidence/cargo-version.txt
sha256sum "$(rustup which rustc)" "$(rustup which cargo)" > /stage/evidence/rust-tool-binary-sha256.txt
cc --version > /stage/evidence/cc-version.txt
ld --version > /stage/evidence/ld-version.txt
cat /etc/os-release > /stage/evidence/rust-os-release.txt
