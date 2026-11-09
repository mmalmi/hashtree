#!/bin/sh
set -eu
prefix=${1:?}
case "$prefix" in base|derived) ;; *) exit 64 ;; esac
rustc -Vv > "/stage/evidence/$prefix-rustc-version.txt"
cargo -V > "/stage/evidence/$prefix-cargo-version.txt"
sha256sum "$(rustup which rustc)" "$(rustup which cargo)" > "/stage/evidence/$prefix-rust-tool-binary-sha256.txt"
cc --version > "/stage/evidence/$prefix-cc-version.txt"
ld --version > "/stage/evidence/$prefix-ld-version.txt"
sha256sum "$(command -v cc)" "$(command -v ld)" > "/stage/evidence/$prefix-cc-ld-binary-sha256.txt"
cat /etc/os-release > "/stage/evidence/$prefix-rust-os-release.txt"
