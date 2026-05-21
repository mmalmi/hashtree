#!/bin/bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: ./publish_release.sh --version <version> [options]

Publish one staged release to the canonical hashtree release tree and mirror the
same staged files to GitHub.

Wrapper options:
  --version <version>       Release tag, for example: v0.2.20
  --skip-github             Publish only to hashtree/Homebrew
  --github-repo <owner/repo>
                            Override the GitHub mirror repo (default: infer from remotes)
  -h, --help                Show this help

All other flags are forwarded to rust/scripts/release_to_htree.sh, including:
  --skip-homebrew-tap
  --cargo-publish
  --release-stage-dir <dir>
  --output-dir <dir>
  --fips-dir <dir>
  --target-dir <dir>
  --targets <csv>
  --windows-artifacts-dir <dir>
  --skip-windows-vm

Examples:
  ./publish_release.sh --version v0.2.20
  ./publish_release.sh --version v0.2.20 --skip-github
  ./publish_release.sh --version v0.2.20 --skip-homebrew-tap
EOF
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$SCRIPT_DIR"
RELEASE_TO_HTREE_SCRIPT="${REPO_DIR}/rust/scripts/release_to_htree.sh"

VERSION=""
SKIP_GITHUB=0
GITHUB_REPO=""
RELEASE_STAGE_DIR=""
FORWARDED_ARGS=()
TEMP_DIRS=()

cleanup() {
    local path
    for path in "${TEMP_DIRS[@]:-}"; do
        if [ -n "$path" ] && [ -e "$path" ]; then
            rm -rf "$path"
        fi
    done
}

trap cleanup EXIT

require_command() {
    local cmd="$1"
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "Missing required command: $cmd" >&2
        exit 1
    fi
}

github_repo_from_url() {
    local url="${1:-}"
    local path=""
    local owner=""
    local repo=""

    case "$url" in
        git@github.com:*)
            path="${url#git@github.com:}"
            ;;
        ssh://git@github.com/*)
            path="${url#ssh://git@github.com/}"
            ;;
        https://github.com/*)
            path="${url#https://github.com/}"
            ;;
        http://github.com/*)
            path="${url#http://github.com/}"
            ;;
        *)
            return 1
            ;;
    esac

    path="${path%/}"
    path="${path%.git}"
    owner="${path%%/*}"
    repo="${path#*/}"
    repo="${repo%%/*}"

    if [ -z "$owner" ] || [ -z "$repo" ] || [ "$owner" = "$repo" ]; then
        return 1
    fi

    printf '%s/%s\n' "$owner" "$repo"
}

infer_github_repo() {
    local remote url repo
    for remote in github upstream origin; do
        url="$(git -C "$REPO_DIR" config --get "remote.${remote}.url" 2>/dev/null || true)"
        repo="$(github_repo_from_url "$url" || true)"
        if [ -n "$repo" ]; then
            printf '%s\n' "$repo"
            return 0
        fi
    done
    return 1
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version)
            VERSION="${2:-}"
            FORWARDED_ARGS+=("$1" "${2:-}")
            shift 2
            ;;
        --release-stage-dir)
            RELEASE_STAGE_DIR="${2:-}"
            FORWARDED_ARGS+=("$1" "${2:-}")
            shift 2
            ;;
        --skip-github)
            SKIP_GITHUB=1
            shift
            ;;
        --github-repo)
            GITHUB_REPO="${2:-}"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            FORWARDED_ARGS+=("$1")
            shift
            ;;
    esac
done

if [ -z "$VERSION" ]; then
    echo "--version is required" >&2
    usage >&2
    exit 1
fi

if [ ! -x "$RELEASE_TO_HTREE_SCRIPT" ]; then
    echo "Missing release publisher: ${RELEASE_TO_HTREE_SCRIPT}" >&2
    exit 1
fi

if [ -z "$RELEASE_STAGE_DIR" ]; then
    RELEASE_STAGE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/hashtree-release-stage-XXXXXX")"
    TEMP_DIRS+=("$RELEASE_STAGE_DIR")
    FORWARDED_ARGS+=(--release-stage-dir "$RELEASE_STAGE_DIR")
fi

if [ "$SKIP_GITHUB" -eq 0 ]; then
    require_command gh
    if [ -z "$GITHUB_REPO" ]; then
        GITHUB_REPO="$(infer_github_repo || true)"
    fi
    if [ -z "$GITHUB_REPO" ]; then
        echo "Could not infer a GitHub repo. Pass --github-repo <owner/repo> or use --skip-github." >&2
        exit 1
    fi
    if ! gh auth status >/dev/null 2>&1; then
        echo "GitHub CLI is not authenticated. Run 'gh auth status' or use --skip-github." >&2
        exit 1
    fi
fi

"$RELEASE_TO_HTREE_SCRIPT" "${FORWARDED_ARGS[@]}"

if [ "$SKIP_GITHUB" -eq 1 ]; then
    exit 0
fi

if [ ! -f "${RELEASE_STAGE_DIR}/notes.md" ]; then
    echo "Missing staged release notes: ${RELEASE_STAGE_DIR}/notes.md" >&2
    exit 1
fi

GITHUB_FILES=()
if [ -f "${RELEASE_STAGE_DIR}/release.json" ]; then
    GITHUB_FILES+=("${RELEASE_STAGE_DIR}/release.json")
fi
if [ -f "${RELEASE_STAGE_DIR}/install.sh" ]; then
    GITHUB_FILES+=("${RELEASE_STAGE_DIR}/install.sh")
fi
if [ -d "${RELEASE_STAGE_DIR}/assets" ]; then
    while IFS= read -r asset_path; do
        if [ -n "$asset_path" ]; then
            GITHUB_FILES+=("$asset_path")
        fi
    done < <(find "${RELEASE_STAGE_DIR}/assets" -maxdepth 1 -type f | LC_ALL=C sort)
fi

if [ "${#GITHUB_FILES[@]}" -eq 0 ]; then
    echo "No staged release files found to mirror to GitHub in ${RELEASE_STAGE_DIR}" >&2
    exit 1
fi

if gh release view "$VERSION" --repo "$GITHUB_REPO" >/dev/null 2>&1; then
    gh release edit "$VERSION" \
        --repo "$GITHUB_REPO" \
        --title "$VERSION" \
        --notes-file "${RELEASE_STAGE_DIR}/notes.md"
    gh release upload "$VERSION" \
        "${GITHUB_FILES[@]}" \
        --repo "$GITHUB_REPO" \
        --clobber
else
    gh release create "$VERSION" \
        "${GITHUB_FILES[@]}" \
        --repo "$GITHUB_REPO" \
        --title "$VERSION" \
        --notes-file "${RELEASE_STAGE_DIR}/notes.md"
fi

cat <<EOF
Mirrored staged release to GitHub:
  https://github.com/${GITHUB_REPO}/releases/tag/${VERSION}
EOF
