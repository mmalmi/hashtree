#!/bin/bash
set -euo pipefail

RUST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$RUST_DIR"

# Resolves features only: no compiler, build scripts, downloads or target install.
# Check consumers separately and together: Cargo features are additive.
for selection in fuse cli combined; do
    case "$selection" in
        fuse) packages=(-p hashtree-fuse); features=hashtree-fuse/fuse ;;
        cli) packages=(-p hashtree-cli); features=hashtree-cli/fuse ;;
        combined) packages=(-p hashtree-fuse -p hashtree-cli)
            features=hashtree-fuse/fuse,hashtree-cli/fuse ;;
    esac
    for target in x86_64-unknown-linux-gnu aarch64-apple-darwin; do
        tree="$(cargo tree --locked --offline "${packages[@]}" --features "$features" \
            --target "$target" -e normal,build --depth 1 -f '{p} [{f}]')"
        fuser="$(printf '%s\n' "$tree" | grep 'hashtree-fuser v')"
        for abi in abi-7-12 abi-7-18; do
            [[ "$fuser" == *"$abi"* ]] || {
                echo "$selection $target missing $abi" >&2; exit 1;
            }
        done
        if [[ "$target" == *linux* ]]; then
            [[ "$fuser" != *libfuse* && "$fuser" != *pkg-config* ]] || {
                echo "$selection $target must use the pure FUSE backend" >&2; exit 1;
            }
        else
            [[ "$fuser" == *libfuse* && "$fuser" == *pkg-config* ]] || {
                echo "$selection $target must retain its libfuse backend" >&2; exit 1;
            }
        fi
        echo "$selection $target passed"
    done
done
