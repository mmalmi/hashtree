#!/bin/bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: packaging/homebrew/publish_tap.sh --version <version> --release-base-url <url> --assets-dir <dir> [options]

Update a Homebrew tap repository without replacing its history, then publish it.
The htree destination must already be provisioned; see packaging/homebrew/README.md.

Required options:
  --version <version>              Release version, for example: v0.2.15
  --release-base-url <url>         Asset base URL containing hashtree-<target>.tar.gz files
  --assets-dir <dir>               Directory containing hashtree-<target>.tar.gz files

Optional:
  --tap-repo <name>                Tap repo name (default: homebrew-<repo-name>)
  --push-url <url>                 Publish destination (default: htree://self/<tap-repo>)
  --npub <npub>                    Npub used only for install URL output
  --target-dir <dir>               Cargo target dir searched for git-remote-htree
  --formula-name <name>            Formula name (default: htree)
  --alias-name <name>              Alias name (default: hashtree)
  --no-alias                       Do not create an alias
  --homepage <url>                 Formula homepage
  --desc <text>                    Formula description
  --license <id>                   Formula license
  -h, --help                       Show this help

Examples:
  packaging/homebrew/publish_tap.sh \
    --version v0.2.15 \
    --release-base-url https://upload.iris.to/<npub>/releases%2Fhashtree/v0.2.15/assets \
    --assets-dir rust/dist/hashtree-v0.2.15
EOF
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"
source "${REPO_DIR}/rust/scripts/release_common.sh"
RUST_DIR="${REPO_DIR}/rust"
CREATE_TAP_SCRIPT="${SCRIPT_DIR}/create_tap.sh"

VERSION=""
RELEASE_BASE_URL=""
ASSETS_DIR=""
TARGET_DIR="${RUST_DIR}/target"
TAP_REPO=""
PUSH_URL=""
NPUB=""
FORMULA_NAME="htree"
ALIAS_NAME="hashtree"
CREATE_ALIAS=1

CREATE_TAP_ARGS=()

require_command() {
    local cmd="$1"
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "Missing required command: $cmd" >&2
        exit 1
    fi
}

default_htree_publish_name() {
    local name="$1"
    if [[ "$name" == *.git ]]; then
        printf '%s\n' "$name"
    else
        printf '%s.git\n' "$name"
    fi
}

htree_publish_name_from_url() {
    local url="$1"
    local name="${url#htree://}"
    name="${name#*/}"
    default_htree_publish_name "$name"
}

refresh_gateway_tap_root_cache() {
    local publish_name="$1"
    local resolve_url

    if [ -z "$NPUB" ]; then
        return 0
    fi
    if ! command -v curl >/dev/null 2>&1; then
        return 0
    fi

    resolve_url="https://upload.iris.to/api/resolve/${NPUB}/$(urlencode_path_segment "$publish_name")?refresh=1"
    if ! curl -fsSL --max-time 30 "$resolve_url" >/dev/null; then
        echo "Warning: gateway tap root cache refresh failed; continuing." >&2
    fi
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version)
            VERSION="${2:-}"
            CREATE_TAP_ARGS+=("$1" "${2:-}")
            shift 2
            ;;
        --release-base-url)
            RELEASE_BASE_URL="${2:-}"
            CREATE_TAP_ARGS+=("$1" "${2:-}")
            shift 2
            ;;
        --assets-dir)
            ASSETS_DIR="${2:-}"
            CREATE_TAP_ARGS+=("$1" "${2:-}")
            shift 2
            ;;
        --checksums-dir)
            ASSETS_DIR="${2:-}"
            CREATE_TAP_ARGS+=(--assets-dir "${2:-}")
            shift 2
            ;;
        --tap-repo)
            TAP_REPO="${2:-}"
            shift 2
            ;;
        --push-url)
            PUSH_URL="${2:-}"
            shift 2
            ;;
        --npub)
            NPUB="${2:-}"
            shift 2
            ;;
        --target-dir)
            TARGET_DIR="${2:-}"
            shift 2
            ;;
        --output-dir)
            echo "--output-dir is managed internally by publish_tap.sh" >&2
            exit 1
            ;;
        --formula-name|--alias-name|--homepage|--desc|--license)
            case "$1" in
                --formula-name)
                    FORMULA_NAME="${2:-}"
                    ;;
                --alias-name)
                    ALIAS_NAME="${2:-}"
                    CREATE_ALIAS=1
                    ;;
            esac
            CREATE_TAP_ARGS+=("$1" "${2:-}")
            shift 2
            ;;
        --no-alias)
            CREATE_ALIAS=0
            CREATE_TAP_ARGS+=("$1")
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

if [ -z "$VERSION" ] || [ -z "$RELEASE_BASE_URL" ] || [ -z "$ASSETS_DIR" ]; then
    usage >&2
    exit 1
fi

if [ ! -d "$ASSETS_DIR" ]; then
    echo "Assets directory does not exist: $ASSETS_DIR" >&2
    exit 1
fi

require_command git
require_command tar
require_command "$CREATE_TAP_SCRIPT"

repo_name="$(infer_repo_name "$REPO_DIR")"
if [ -z "$TAP_REPO" ]; then
    TAP_REPO="homebrew-${repo_name}"
fi

if [ -z "$PUSH_URL" ]; then
    PUSH_URL="htree://self/${TAP_REPO}"
fi

if [ -z "$NPUB" ] && command -v htree >/dev/null 2>&1; then
    NPUB="$(current_npub)"
fi

tmp_dir="$(mktemp -d)"
bare_repo="${tmp_dir}/tap.git"
work_repo="${tmp_dir}/work"
trap 'rm -rf "$tmp_dir"' EXIT

generated_repo="${tmp_dir}/generated.git"
"${CREATE_TAP_SCRIPT}" \
    "${CREATE_TAP_ARGS[@]}" \
    --output-dir "${generated_repo}" >/dev/null

gateway_url=""
canonical_url=""
if [[ "$PUSH_URL" == htree://* ]]; then
    require_command htree
    publisher_npub="$(htree user | awk 'NR == 1 { print $1 }')"
    if [ -z "$publisher_npub" ]; then
        echo "Cannot identify the tap publisher" >&2
        exit 1
    fi
    authority="${PUSH_URL#htree://}"
    authority="${authority%%/*}"
    if [[ "$authority" != self && "$authority" != "$publisher_npub" ]]; then
        echo "Tap destination must belong to the current publisher" >&2
        exit 1
    fi
    publish_name="$(htree_publish_name_from_url "$PUSH_URL")"
    source_url="htree://${publisher_npub}/${publish_name}"
    # This tree is a bare Git directory, not git-remote-htree's .git layout.
    # A lookup or download failure must never be treated as a new tap.
    htree get "$source_url" --output "$bare_repo" >/dev/null
    canonical_url="htree://self/${publish_name}"
    if [ -n "$NPUB" ]; then
        gateway_url="https://upload.iris.to/${NPUB}/${publish_name}"
    fi
else
    git clone --mirror "$PUSH_URL" "$bare_repo" >/dev/null
fi

test "$(git --git-dir="$bare_repo" rev-parse --is-bare-repository)" = true
git --git-dir="$bare_repo" fsck --full --strict >/dev/null
git --git-dir="$bare_repo" for-each-ref --format='%(objectname) %(refname)' >"${tmp_dir}/prior-refs"
if [ -s "${tmp_dir}/prior-refs" ]; then
    # Do not silently replace an unexpected branch layout with an orphan master.
    git --git-dir="$bare_repo" rev-parse --verify refs/heads/master >/dev/null
    test "$(git --git-dir="$bare_repo" symbolic-ref HEAD)" = refs/heads/master
else
    git --git-dir="$bare_repo" symbolic-ref HEAD refs/heads/master
fi
git clone "$bare_repo" "$work_repo" >/dev/null
# Overlay only generated formula/alias files, retaining other tap content.
git --git-dir="$generated_repo" archive master | tar -x -C "$work_repo"
if [ "$CREATE_ALIAS" -eq 0 ] && [ -L "${work_repo}/Aliases/${ALIAS_NAME}" ] && \
    [ "$(readlink "${work_repo}/Aliases/${ALIAS_NAME}")" = "../Formula/${FORMULA_NAME}.rb" ]; then
    rm "${work_repo}/Aliases/${ALIAS_NAME}"
fi
git -C "$work_repo" add -A
if git -C "$work_repo" diff --cached --quiet; then
    echo "Homebrew tap already matches ${VERSION}; nothing to publish."
    exit 0
else
    git -C "$work_repo" -c user.name='Codex' -c user.email='codex@example.com' \
        commit -m "Update ${FORMULA_NAME} to ${VERSION}" >/dev/null
    git -C "$work_repo" push origin master >/dev/null
fi
git --git-dir="$bare_repo" fsck --full --strict >/dev/null
git --git-dir="$bare_repo" update-server-info

if [[ "$PUSH_URL" == htree://* ]]; then
    # Fail if another publisher changed any ref while this update was prepared.
    latest_repo="${tmp_dir}/latest.git"
    htree get "$source_url" --output "$latest_repo" >/dev/null
    git --git-dir="$latest_repo" fsck --full --strict >/dev/null
    git --git-dir="$latest_repo" for-each-ref --format='%(objectname) %(refname)' >"${tmp_dir}/latest-refs"
    cmp "${tmp_dir}/prior-refs" "${tmp_dir}/latest-refs"
    cmp "${bare_repo}/HEAD" "${latest_repo}/HEAD"
    while IFS= read -r remote; do
        git --git-dir="$bare_repo" config --remove-section "remote.$remote"
    done < <(git --git-dir="$bare_repo" remote)
    (
        cd "${REPO_DIR}"
        htree add "${bare_repo}" --publish "${publish_name}" >/dev/null
    )
    refresh_gateway_tap_root_cache "$publish_name"
else
    git --git-dir="$bare_repo" push "$PUSH_URL" master >/dev/null
fi

echo "Published Homebrew tap."

if [ -n "$canonical_url" ]; then
    cat <<EOF

Canonical:
  ${canonical_url}
EOF
fi

if [ -n "$NPUB" ]; then
    if [ -z "$gateway_url" ]; then
        gateway_url="https://upload.iris.to/${NPUB}/${TAP_REPO}/.git"
    fi
    cat <<EOF

Gateway URL:
  ${gateway_url}

Install:
  brew tap <user>/<repo> ${gateway_url}
  brew trust --tap <user>/<repo>
  brew install ${FORMULA_NAME}
EOF

    if [ "$CREATE_ALIAS" -eq 1 ] && [ -n "$ALIAS_NAME" ] && [ "$ALIAS_NAME" != "$FORMULA_NAME" ]; then
        cat <<EOF

Alias:
  brew install ${ALIAS_NAME}
EOF
    fi
fi
