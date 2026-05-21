#!/bin/bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: rust/scripts/release_to_htree.sh --version <version> [options]

Builds local CLI release artifacts, stages a metadata-backed repo release
directory, adds it to hashtree, then publishes it into a mutable release tree.

Options:
  --version <version>                 Release version label, for example: v0.2.3
  --version-path <path>              Published path inside the release tree (default: <version>)
  --tree-name <name>                 Mutable release tree name (default: releases/<repo>)
  --homebrew-tap-repo <name>         Homebrew tap repo name (default: homebrew-<repo>)
  --skip-homebrew-tap                Skip updating the Homebrew tap
  --skip-post-publish-install-checks Skip live install smoke checks after publish
  --cargo-publish                    Publish Rust crates to crates.io after releasing artifacts
  --output-dir <dir>                 Release directory to create/use
  --repo-dir <dir>                   Repository root to build/package from
  --fips-dir <dir>                   FIPS repository root for Docker Linux builds
  --target-dir <dir>                 Cargo target dir to read/write
  --targets <csv>                    Comma-separated targets to build/package
  --windows-artifacts-dir <dir>      Directory containing Windows .exe binaries from a VM
  --skip-windows-vm                  Skip auto-building Windows CLI artifacts on win11-dev
  --windows-vm-name <name>           SSH host running Windows (default: win11-dev). Legacy alias.
  --windows-ssh-host <host>          SSH host running Windows (default: win11-dev)
  --windows-shared-repo-path <path>  Ignored (no longer using Parallels shared folders)
  --windows-guest-repo-path <path>   Override the guest repo path used for the Windows build
  --package-only                     Skip builds and package existing binaries only
  --linux-builder <mode>             Linux musl builder for release artifacts: auto, cross, or docker
  --docker-bin <path>                Docker binary to use for Linux docker builds
  --docker-rust-image <image>        Rust Alpine image to use for Linux docker builds
  --release-stage-dir <dir>          Directory to use for the staged repo release metadata
  --cargo-bin <path>                 Cargo binary to use
  --cross-bin <path>                 cross binary to use for Linux musl targets
  -h, --help                         Show this help

Examples:
  rust/scripts/release_to_htree.sh --version v0.2.3
  rust/scripts/release_to_htree.sh --version v0.2.3 --windows-artifacts-dir /Volumes/windows-share/release
EOF
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/release_common.sh"
RUST_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
REPO_DIR="$(cd "${RUST_DIR}/.." && pwd)"
REPO_NAME="$(infer_repo_name "$REPO_DIR")"
CHANGELOG_FILE="${RUST_DIR}/CHANGELOG.md"

VERSION=""
VERSION_PATH=""
TREE_NAME="releases/${REPO_NAME}"
HOMEBREW_TAP_REPO="homebrew-${REPO_NAME}"
SKIP_HOMEBREW_TAP=0
SKIP_POST_PUBLISH_INSTALL_CHECKS=0
CARGO_PUBLISH=0
RELEASE_STAGE_DIR=""

BUILD_ARGS=()
TEMP_DIRS=()
SKIP_WINDOWS_VM=0
WINDOWS_VM_NAME=""
WINDOWS_SHARED_REPO_PATH=""
WINDOWS_GUEST_REPO_PATH=""

cleanup() {
    local path
    for path in "${TEMP_DIRS[@]:-}"; do
        if [ -n "$path" ] && [ -e "$path" ]; then
            rm -rf "$path"
        fi
    done
}

trap cleanup EXIT

while [ $# -gt 0 ]; do
    case "$1" in
        --version)
            VERSION="${2:-}"
            BUILD_ARGS+=("$1" "${2:-}")
            shift 2
            ;;
        --version-path)
            VERSION_PATH="${2:-}"
            shift 2
            ;;
        --tree-name)
            TREE_NAME="${2:-}"
            shift 2
            ;;
        --homebrew-tap-repo)
            HOMEBREW_TAP_REPO="${2:-}"
            shift 2
            ;;
        --skip-homebrew-tap)
            SKIP_HOMEBREW_TAP=1
            shift
            ;;
        --skip-post-publish-install-checks)
            SKIP_POST_PUBLISH_INSTALL_CHECKS=1
            shift
            ;;
        --cargo-publish)
            CARGO_PUBLISH=1
            shift
            ;;
        --skip-windows-vm)
            SKIP_WINDOWS_VM=1
            shift
            ;;
        --windows-vm-name|--windows-ssh-host)
            WINDOWS_VM_NAME="${2:-}"
            shift 2
            ;;
        --windows-shared-repo-path)
            WINDOWS_SHARED_REPO_PATH="${2:-}"
            shift 2
            ;;
        --windows-guest-repo-path)
            WINDOWS_GUEST_REPO_PATH="${2:-}"
            shift 2
            ;;
        --release-stage-dir)
            RELEASE_STAGE_DIR="${2:-}"
            shift 2
            ;;
        --output-dir|--repo-dir|--fips-dir|--target-dir|--targets|--windows-artifacts-dir|--cargo-bin|--cross-bin|--linux-builder|--docker-bin|--docker-rust-image)
            BUILD_ARGS+=("$1" "${2:-}")
            shift 2
            ;;
        --package-only)
            BUILD_ARGS+=("$1")
            shift
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

if [ -z "$VERSION_PATH" ]; then
    VERSION_PATH="$VERSION"
fi

value_from_build_args() {
    local key="$1"
    local default_value="${2:-}"
    local i
    for ((i = 0; i < ${#BUILD_ARGS[@]}; i++)); do
        if [ "${BUILD_ARGS[$i]}" = "$key" ]; then
            echo "${BUILD_ARGS[$((i + 1))]}"
            return
        fi
    done
    echo "$default_value"
}

require_command() {
    local cmd="$1"
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "Missing required command: $cmd" >&2
        exit 1
    fi
}

homebrew_archives_ready() {
    local assets_dir="$1"
    local target
    for target in \
        aarch64-apple-darwin \
        x86_64-apple-darwin \
        aarch64-unknown-linux-musl \
        x86_64-unknown-linux-musl
    do
        if [ ! -f "${assets_dir}/hashtree-${target}.tar.gz" ]; then
            return 1
        fi
    done
    return 0
}

require_homebrew_archives_for_release() {
    local assets_dir="$1"
    if [ "$SKIP_HOMEBREW_TAP" -eq 1 ]; then
        return 0
    fi
    if homebrew_archives_ready "$assets_dir"; then
        return 0
    fi

    cat >&2 <<EOF
Error: release directory ${assets_dir} does not contain the full macOS/Linux archive set required for the Homebrew tap.
Re-run with the default targets (or explicit macOS and Linux musl targets), or pass --skip-homebrew-tap to publish a partial release intentionally.
EOF
    exit 1
}

auto_build_windows_vm_artifacts() {
    local helper_script windows_output_dir

    if [ "$SKIP_WINDOWS_VM" -eq 1 ]; then
        return
    fi

    if [ -n "$(value_from_build_args --windows-artifacts-dir)" ]; then
        return
    fi

    helper_script="${SCRIPT_DIR}/build_windows_vm_artifacts.mjs"
    if [ ! -f "$helper_script" ]; then
        echo "Warning: Windows VM build helper not found at ${helper_script}; skipping Windows CLI artifacts." >&2
        return
    fi

    if ! command -v node >/dev/null 2>&1; then
        echo "Warning: node is required for Windows VM builds; skipping Windows CLI artifacts." >&2
        return
    fi

    mkdir -p "${RUST_DIR}/dist"
    windows_output_dir="$(mktemp -d "${RUST_DIR}/dist/windows-vm-XXXXXX")"
    TEMP_DIRS+=("$windows_output_dir")

    WINDOWS_BUILD_ARGS=("$helper_script" "--output-dir" "$windows_output_dir")
    if [ -n "$WINDOWS_VM_NAME" ]; then
        WINDOWS_BUILD_ARGS+=("--ssh-host" "$WINDOWS_VM_NAME")
    fi
    # WINDOWS_SHARED_REPO_PATH is ignored under the SSH flow.
    if [ -n "$WINDOWS_GUEST_REPO_PATH" ]; then
        WINDOWS_BUILD_ARGS+=("--guest-repo-path" "$WINDOWS_GUEST_REPO_PATH")
    fi

    if node "${WINDOWS_BUILD_ARGS[@]}"; then
        BUILD_ARGS+=("--windows-artifacts-dir" "$windows_output_dir")
    else
        echo "Warning: Windows VM build failed; continuing without Windows CLI artifacts." >&2
        rm -rf "$windows_output_dir"
    fi
}

auto_build_windows_vm_artifacts

"${SCRIPT_DIR}/build_release_artifacts.sh" "${BUILD_ARGS[@]}"

OUTPUT_DIR="$(value_from_build_args --output-dir "${RUST_DIR}/dist/hashtree-${VERSION}")"
TARGET_DIR="$(value_from_build_args --target-dir "${RUST_DIR}/target")"
BUILD_REPO_DIR="$(value_from_build_args --repo-dir "${REPO_DIR}")"
require_homebrew_archives_for_release "$OUTPUT_DIR"
npub="$(current_npub)"
RELEASE_STAGE_SCRIPT="${REPO_DIR}/scripts/stage_repo_release.mjs"
RELEASE_BOOTSTRAP_SCRIPT="${SCRIPT_DIR}/write_release_bootstrap_installer.sh"

if [ -n "$npub" ]; then
    if ! homebrew_archives_ready "$OUTPUT_DIR"; then
        echo "Warning: skipping release installer generation because the full macOS/Linux archive set is not present." >&2
    elif [ ! -x "$RELEASE_BOOTSTRAP_SCRIPT" ]; then
        echo "Missing release bootstrap helper: ${RELEASE_BOOTSTRAP_SCRIPT}" >&2
        exit 1
    else
        "${RELEASE_BOOTSTRAP_SCRIPT}" \
            --path "${OUTPUT_DIR}/install.sh" \
            --base-url "$(gateway_release_base_url "$npub" "$TREE_NAME" "$VERSION_PATH")"
    fi
else
    echo "Warning: Could not determine current npub; skipping release installer generation." >&2
fi

if [ ! -f "$RELEASE_STAGE_SCRIPT" ]; then
    echo "Missing repo release staging helper: ${RELEASE_STAGE_SCRIPT}" >&2
    exit 1
fi

require_command node
if [ -z "$RELEASE_STAGE_DIR" ]; then
    RELEASE_STAGE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/hashtree-release-stage-XXXXXX")"
    TEMP_DIRS+=("$RELEASE_STAGE_DIR")
fi
RELEASE_COMMIT="$(git -C "$REPO_DIR" rev-parse HEAD 2>/dev/null || printf '%s\n' HEAD)"
RELEASE_COMMIT="$(git -C "$BUILD_REPO_DIR" rev-parse HEAD 2>/dev/null || printf '%s\n' "$RELEASE_COMMIT")"
STAGE_ARGS=(
    "$RELEASE_STAGE_SCRIPT"
    --tag "$VERSION"
    --commit "$RELEASE_COMMIT"
    --cli-dir "$OUTPUT_DIR"
    --output-dir "$RELEASE_STAGE_DIR"
    --changelog-file "$CHANGELOG_FILE"
)

if [ -n "$npub" ] && [ -f "${OUTPUT_DIR}/install.sh" ]; then
    STAGE_ARGS+=(--install-url "$(gateway_release_base_url "$npub" "$TREE_NAME" "$VERSION_PATH")/install.sh")
fi

node "${STAGE_ARGS[@]}"

release_cid="$(
    cd "$REPO_DIR"
    htree add "$RELEASE_STAGE_DIR" | awk '/^  url:/ {print $2}'
)"

if [ -z "$release_cid" ]; then
    echo "Failed to determine release CID from htree add output" >&2
    exit 1
fi

echo "Release CID: ${release_cid}"
"${SCRIPT_DIR}/publish_release.sh" "$VERSION_PATH" "$release_cid" "$TREE_NAME"

if [ "$SKIP_HOMEBREW_TAP" -eq 0 ]; then
    HOMEBREW_PUBLISH_SCRIPT="${REPO_DIR}/packaging/homebrew/publish_tap.sh"
    if [ ! -x "$HOMEBREW_PUBLISH_SCRIPT" ]; then
        echo "Warning: Homebrew tap script not found at packaging/homebrew/publish_tap.sh; skipping tap update." >&2
    else
        if [ -z "$npub" ]; then
            echo "Warning: Could not determine current npub; skipping Homebrew tap update." >&2
        else
            release_base_url="$(gateway_release_base_url "$npub" "$TREE_NAME" "$VERSION_PATH")/assets"
            if ! "${HOMEBREW_PUBLISH_SCRIPT}" \
                --version "$VERSION" \
                --release-base-url "$release_base_url" \
                --assets-dir "$OUTPUT_DIR" \
                --tap-repo "$HOMEBREW_TAP_REPO" \
                --target-dir "$TARGET_DIR"
            then
                echo "Warning: Homebrew tap update failed; release artifacts are still published." >&2
            fi
        fi
    fi
fi

run_post_publish_install_checks() {
    local install_matrix_script

    if [ "$SKIP_POST_PUBLISH_INSTALL_CHECKS" -eq 1 ]; then
        return 0
    fi

    install_matrix_script="${SCRIPT_DIR}/test_install_matrix.sh"
    if [ ! -x "$install_matrix_script" ]; then
        echo "Warning: post-publish install matrix helper not found at ${install_matrix_script}; skipping live install checks." >&2
        return 0
    fi

    echo "Running post-publish install matrix against live artifacts..."
    if ! "$install_matrix_script"; then
        echo "Warning: post-publish install checks reported failures; release artifacts remain published." >&2
    fi
}

run_post_publish_install_checks

if [ "$CARGO_PUBLISH" -eq 1 ]; then
    "${SCRIPT_DIR}/publish.sh"
fi
