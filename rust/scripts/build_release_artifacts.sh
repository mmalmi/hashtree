#!/bin/bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: rust/scripts/build_release_artifacts.sh --version <version> [options]

Builds and packages CLI release artifacts in the same layout as the GitHub release
workflow for the supported local targets, then writes them into a release directory.

Options:
  --version <version>                 Release version label, for example: v0.2.3
  --repo-dir <dir>                   Repository root to build/package from (default: current checkout)
  --output-dir <dir>                 Output directory (default: rust/dist/hashtree-<version>)
  --target-dir <dir>                 Cargo target dir to read/write (default: rust/target)
  --targets <csv>                    Comma-separated targets to package
  --windows-artifacts-dir <dir>      Directory containing Windows .exe binaries from a VM
  --package-only                     Skip builds and package existing binaries only
  --cargo-bin <path>                 Cargo binary to use (default: cargo)
  --cross-bin <path>                 cross binary to use for Linux musl targets (default: cross)
  --linux-builder <mode>             Linux musl builder: auto, cross, or docker (default: auto)
  --docker-bin <path>                Docker binary to use for Linux docker builds (default: docker)
  --docker-rust-image <image>        Rust Alpine image to use for Linux docker builds
  -h, --help                         Show this help

Examples:
  rust/scripts/build_release_artifacts.sh --version v0.2.3
  rust/scripts/build_release_artifacts.sh --version v0.2.3 --targets aarch64-apple-darwin,x86_64-unknown-linux-musl
  rust/scripts/build_release_artifacts.sh --version v0.2.3 --windows-artifacts-dir /Volumes/windows-share/release
  rust/scripts/build_release_artifacts.sh --version v0.2.3 --package-only --target-dir /tmp/target
EOF
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEFAULT_RUST_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
DEFAULT_REPO_DIR="$(cd "${DEFAULT_RUST_DIR}/.." && pwd)"

VERSION=""
REPO_DIR="${DEFAULT_REPO_DIR}"
RUST_DIR="${DEFAULT_RUST_DIR}"
OUTPUT_DIR=""
TARGET_DIR=""
TARGETS_CSV=""
WINDOWS_ARTIFACTS_DIR=""
PACKAGE_ONLY=0
CARGO_BIN="${CARGO_BIN:-cargo}"
CROSS_BIN="${CROSS_BIN:-cross}"
LINUX_BUILDER="${LINUX_BUILDER:-auto}"
DOCKER_BIN="${DOCKER_BIN:-docker}"
DOCKER_RUST_IMAGE="${DOCKER_RUST_IMAGE:-}"

default_targets_csv() {
    case "$(uname -s)" in
        Darwin)
            echo "aarch64-apple-darwin,x86_64-apple-darwin,x86_64-unknown-linux-musl,aarch64-unknown-linux-musl"
            ;;
        Linux)
            echo "x86_64-unknown-linux-musl,aarch64-unknown-linux-musl"
            ;;
        *)
            echo ""
            ;;
    esac
}

require_command() {
    local cmd="$1"
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "Missing required command: $cmd" >&2
        exit 1
    fi
}

target_release_features() {
    case "$1" in
        x86_64-unknown-linux-musl|aarch64-unknown-linux-musl)
            # Release artifacts omit optional FUSE support; source builds can
            # opt in with `--features lmdb,fuse` on systems with libfuse.
            printf '%s\n' ""
            ;;
        x86_64-apple-darwin|aarch64-apple-darwin)
            # macOS release artifacts intentionally omit FUSE so `htree` still
            # launches on systems without macFUSE installed.
            printf '%s\n' ""
            ;;
        *)
            printf '%s\n' ""
            ;;
    esac
}

resolve_linux_builder() {
    case "$LINUX_BUILDER" in
        auto)
            # Prefer target-native Alpine containers for Linux release artifacts.
            # This keeps the release environment predictable without enabling
            # optional platform integrations in published binaries.
            if command -v "$DOCKER_BIN" >/dev/null 2>&1; then
                printf '%s\n' docker
            else
                printf '%s\n' cross
            fi
            ;;
        cross|docker)
            printf '%s\n' "$LINUX_BUILDER"
            ;;
        *)
            echo "Unsupported --linux-builder value: ${LINUX_BUILDER}" >&2
            exit 1
            ;;
    esac
}

build_linux_target_with_docker() {
    local target="$1"
    local helper_script="${SCRIPT_DIR}/build_linux_release_target_docker.sh"
    local args=(
        --target "$target"
        --repo-dir "$REPO_DIR"
        --target-dir "$TARGET_DIR"
        --docker-bin "$DOCKER_BIN"
        --cargo-bin "$CARGO_BIN"
    )

    if [ ! -x "$helper_script" ]; then
        echo "Missing Linux Docker build helper: ${helper_script}" >&2
        exit 1
    fi

    if [ -n "$DOCKER_RUST_IMAGE" ]; then
        args+=(--docker-rust-image "$DOCKER_RUST_IMAGE")
    fi
    echo "Building ${target} with Docker-native musl toolchain"
    "$helper_script" "${args[@]}"
}

write_unix_install_script() {
    local path="$1"
    cat >"$path" <<'EOF'
#!/bin/bash
set -e

default_install_dir() {
  if [ "${1:-}" = "root" ]; then
    printf '%s\n' /usr/local/bin
    return
  fi

  if [ -n "${XDG_BIN_HOME:-}" ]; then
    printf '%s\n' "${XDG_BIN_HOME}"
    return
  fi

  if [ -n "${HOME:-}" ]; then
    printf '%s\n' "${HOME}/.local/bin"
    return
  fi

  printf '%s\n' /usr/local/bin
}

path_contains() {
  local target="$1"
  local entry
  local old_ifs="${IFS}"
  IFS=:
  for entry in ${PATH:-}; do
    if [ "$entry" = "$target" ]; then
      IFS="${old_ifs}"
      return 0
    fi
  done
  IFS="${old_ifs}"
  return 1
}

existing_parent_dir() {
  local dir="$1"
  while [ ! -e "$dir" ]; do
    dir="$(dirname "$dir")"
  done
  printf '%s\n' "$dir"
}

if [ $# -gt 0 ]; then
  INSTALL_DIR="$1"
elif [ "$(id -u)" -eq 0 ]; then
  INSTALL_DIR="$(default_install_dir root)"
else
  INSTALL_DIR="$(default_install_dir)"
fi

echo "Installing hashtree binaries to $INSTALL_DIR"

if [ ! -d "$INSTALL_DIR" ]; then
  EXISTING_PARENT="$(existing_parent_dir "$INSTALL_DIR")"
  if [ -w "$EXISTING_PARENT" ]; then
    mkdir -p "$INSTALL_DIR"
  else
    echo "Need sudo to create $INSTALL_DIR"
    sudo mkdir -p "$INSTALL_DIR"
  fi
fi

if [ ! -w "$INSTALL_DIR" ]; then
  echo "Need sudo to install to $INSTALL_DIR"
  sudo install -m 755 htree htree-cashu git-remote-htree "$INSTALL_DIR/"
else
  install -m 755 htree htree-cashu git-remote-htree "$INSTALL_DIR/"
fi

echo "✓ Installed htree, htree-cashu, and git-remote-htree"
echo ""
echo "Note: release binaries omit optional FUSE mount support."
echo "Build from source with: cargo install hashtree-cli --no-default-features --features lmdb,fuse"
if ! path_contains "$INSTALL_DIR"; then
  echo ""
  echo "Add $INSTALL_DIR to your PATH, for example:"
  echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
fi
echo ""
echo "Verify with:"
echo "  htree --help"
echo "  htree cashu balance"
echo "  git clone htree://npub1.../repo"
EOF
    chmod +x "$path"
}

write_unix_readme() {
    local path="$1"
    cat >"$path" <<'EOF'
hashtree - Git over Nostr via Merkle trees
==========================================

Binaries included:
  htree             - CLI and daemon for hashtree operations
  htree-cashu       - Cashu wallet helper for htree cashu
  git-remote-htree  - Git remote helper for htree:// URLs

Quick install:
  ./install.sh               # installs to ~/.local/bin by default
  ./install.sh /usr/local/bin # installs system-wide (may need sudo)

Manual install:
  cp htree htree-cashu git-remote-htree ~/.local/bin/

Usage:
  htree add <file>                    # add file to hashtree
  htree get <hash>                    # download by hash
  htree start                         # start P2P daemon
  htree cashu balance                 # inspect Cashu wallet
  git clone htree://npub1.../repo     # clone git repo
  git remote add htree htree://self/myrepo
  git push htree main

FUSE note:
  Release binaries omit optional FUSE mount support. Build from source with:
    cargo install hashtree-cli --no-default-features --features lmdb,fuse

More info: https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/hashtree
EOF
}

write_windows_readme() {
    local path="$1"
    cat >"$path" <<'EOF'
hashtree - Git over Nostr via Merkle trees
==========================================

Binaries included:
  htree.exe             - CLI and daemon for hashtree operations
  htree-cashu.exe       - Cashu wallet helper for htree cashu
  git-remote-htree.exe  - Git remote helper for htree:// URLs

Install:
  Copy all three .exe files to a directory in your PATH, e.g.:
  C:\Users\<you>\AppData\Local\Microsoft\WindowsApps\

Usage:
  htree add <file>                    # add file to hashtree
  htree get <hash>                    # download by hash
  htree start                         # start P2P daemon
  htree cashu balance                 # inspect Cashu wallet
  git clone htree://npub1.../repo     # clone git repo

FUSE note:
  Prebuilt release binaries omit optional FUSE mount support. Build from source with:
    cargo install hashtree-cli --no-default-features --features lmdb,fuse

More info: https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/hashtree
EOF
}

ensure_rust_target() {
    local target="$1"
    if [ "$PACKAGE_ONLY" -eq 1 ]; then
        return
    fi
    require_command rustup
    rustup target add "$target" >/dev/null
}

build_target() {
    local target="$1"
    local release_features
    release_features="$(target_release_features "$target")"
    local cargo_args=(
        build
        --release
        --target "$target"
        -p git-remote-htree
        -p hashtree-cashu-cli
        -p hashtree-cli
    )

    if [ -n "$release_features" ]; then
        cargo_args+=(--features "$release_features")
    fi

    if [ "$PACKAGE_ONLY" -eq 1 ]; then
        return
    fi

    if [ -f "${RUST_DIR}/Cargo.lock" ]; then
        cargo_args+=(--locked)
    fi

    case "$target" in
        x86_64-unknown-linux-musl|aarch64-unknown-linux-musl)
            case "$(resolve_linux_builder)" in
                docker)
                    build_linux_target_with_docker "$target"
                    ;;
                cross)
                    require_command "$CROSS_BIN"
                    (
                        cd "$RUST_DIR"
                        export CARGO_TARGET_DIR="$TARGET_DIR"
                        "$CROSS_BIN" "${cargo_args[@]}"
                    )
                    ;;
            esac
            ;;
        x86_64-apple-darwin|aarch64-apple-darwin)
            local host_arch target_arch
            if [ "$(uname -s)" != "Darwin" ]; then
                echo "Cannot build $target natively on $(uname -s). Use --targets to skip it." >&2
                exit 1
            fi
            require_command "$CARGO_BIN"
            ensure_rust_target "$target"
            host_arch="$(uname -m)"
            target_arch="${target%%-*}"
            (
                cd "$RUST_DIR"
                export CARGO_TARGET_DIR="$TARGET_DIR"
                if [ "$host_arch" != "$target_arch" ]; then
                    export PKG_CONFIG_ALLOW_CROSS=1
                fi
                "$CARGO_BIN" "${cargo_args[@]}"
            )
            ;;
        x86_64-pc-windows-msvc)
            echo "Windows MSVC artifacts must come from a Windows VM or runner via --windows-artifacts-dir." >&2
            exit 1
            ;;
        *)
            echo "Unsupported target: $target" >&2
            exit 1
            ;;
    esac
}

package_unix_target() {
    local target="$1"
    local release_dir="${TARGET_DIR}/${target}/release"
    local stage_dir
    stage_dir="$(mktemp -d)"
    local package_dir="${stage_dir}/hashtree"

    mkdir -p "$package_dir"

    for binary in git-remote-htree htree-cashu htree; do
        if [ ! -f "${release_dir}/${binary}" ]; then
            echo "Missing binary for ${target}: ${release_dir}/${binary}" >&2
            exit 1
        fi
        cp "${release_dir}/${binary}" "${package_dir}/"
    done

    write_unix_install_script "${package_dir}/install.sh"
    write_unix_readme "${package_dir}/README.txt"

    (
        cd "$stage_dir"
        tar -czf "${OUTPUT_DIR}/hashtree-${target}.tar.gz" hashtree
    )

    rm -rf "$stage_dir"
}

package_windows_artifacts() {
    local stage_dir
    stage_dir="$(mktemp -d)"
    local package_dir="${stage_dir}/hashtree"
    mkdir -p "$package_dir"

    for binary in git-remote-htree.exe htree-cashu.exe htree.exe; do
        if [ ! -f "${WINDOWS_ARTIFACTS_DIR}/${binary}" ]; then
            echo "Missing Windows binary: ${WINDOWS_ARTIFACTS_DIR}/${binary}" >&2
            exit 1
        fi
        cp "${WINDOWS_ARTIFACTS_DIR}/${binary}" "${package_dir}/"
    done

    write_windows_readme "${package_dir}/README.txt"

    require_command python3
    (
        cd "$stage_dir"
        python3 - <<'PY'
import pathlib
import zipfile

root = pathlib.Path("hashtree")
with zipfile.ZipFile("hashtree-x86_64-pc-windows-msvc.zip", "w", compression=zipfile.ZIP_DEFLATED) as zf:
    for path in sorted(root.rglob("*")):
        if path.is_file():
            zf.write(path, path.as_posix())
PY
        mv "hashtree-x86_64-pc-windows-msvc.zip" "${OUTPUT_DIR}/"
    )

    rm -rf "$stage_dir"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version)
            VERSION="${2:-}"
            shift 2
            ;;
        --repo-dir)
            REPO_DIR="${2:-}"
            shift 2
            ;;
        --output-dir)
            OUTPUT_DIR="${2:-}"
            shift 2
            ;;
        --target-dir)
            TARGET_DIR="${2:-}"
            shift 2
            ;;
        --targets)
            TARGETS_CSV="${2:-}"
            shift 2
            ;;
        --windows-artifacts-dir)
            WINDOWS_ARTIFACTS_DIR="${2:-}"
            shift 2
            ;;
        --package-only)
            PACKAGE_ONLY=1
            shift
            ;;
        --cargo-bin)
            CARGO_BIN="${2:-}"
            shift 2
            ;;
        --cross-bin)
            CROSS_BIN="${2:-}"
            shift 2
            ;;
        --linux-builder)
            LINUX_BUILDER="${2:-}"
            shift 2
            ;;
        --docker-bin)
            DOCKER_BIN="${2:-}"
            shift 2
            ;;
        --docker-rust-image)
            DOCKER_RUST_IMAGE="${2:-}"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "Unknown argument: $1" >&2
            usage >&2
            exit 1
            ;;
    esac
done

if [ -z "$VERSION" ]; then
    echo "--version is required" >&2
    usage >&2
    exit 1
fi

REPO_DIR="$(cd "$REPO_DIR" && pwd)"
RUST_DIR="${REPO_DIR}/rust"

if [ ! -d "$RUST_DIR" ]; then
    echo "Missing rust workspace in repo dir: ${RUST_DIR}" >&2
    exit 1
fi

if [ -z "$TARGET_DIR" ]; then
    TARGET_DIR="${RUST_DIR}/target"
fi

if [ -z "$TARGETS_CSV" ]; then
    TARGETS_CSV="$(default_targets_csv)"
fi

if [ -z "$TARGETS_CSV" ] && [ -z "$WINDOWS_ARTIFACTS_DIR" ]; then
    echo "No default targets for $(uname -s). Pass --targets explicitly." >&2
    exit 1
fi

if [ -z "$OUTPUT_DIR" ]; then
    OUTPUT_DIR="${RUST_DIR}/dist/hashtree-${VERSION}"
fi

mkdir -p "$(dirname "$OUTPUT_DIR")"
OUTPUT_DIR="$(cd "$(dirname "$OUTPUT_DIR")" && pwd)/$(basename "$OUTPUT_DIR")"
mkdir -p "$TARGET_DIR"
TARGET_DIR="$(cd "$TARGET_DIR" && pwd)"

require_command tar
require_command python3

IFS=',' read -r -a TARGETS <<<"$TARGETS_CSV"

rm -rf "$OUTPUT_DIR"
mkdir -p "$OUTPUT_DIR"

echo "Release version: ${VERSION}"
echo "Output dir: ${OUTPUT_DIR}"
echo "Target dir: ${TARGET_DIR}"
echo "Linux builder: $(resolve_linux_builder)"

if [ "${#TARGETS[@]}" -gt 0 ] && [ -n "${TARGETS[0]}" ]; then
    echo "Targets: ${TARGETS[*]}"
    for target in "${TARGETS[@]}"; do
        build_target "$target"
        package_unix_target "$target"
    done
fi

if [ -n "$WINDOWS_ARTIFACTS_DIR" ]; then
    echo "Including Windows artifacts from: ${WINDOWS_ARTIFACTS_DIR}"
    package_windows_artifacts
fi

echo ""
echo "Created release artifacts in ${OUTPUT_DIR}:"
find "$OUTPUT_DIR" -maxdepth 1 -type f | sort
