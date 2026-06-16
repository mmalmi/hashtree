#!/bin/bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: packaging/homebrew/create_tap.sh --version <version> --release-base-url <url> --assets-dir <dir> --output-dir <dir> [options]

Generate a Homebrew tap as a bare Git repository that can be published on a
static HTTP host.

Required options:
  --version <version>              Release version, for example: v0.2.15
  --release-base-url <url>         Asset base URL containing hashtree-<target>.tar.gz files
  --assets-dir <dir>               Directory containing hashtree-<target>.tar.gz files
  --output-dir <dir>               Output directory for the bare tap repository

Optional:
  --formula-name <name>            Formula name (default: htree)
  --alias-name <name>              Alias name (default: hashtree)
  --no-alias                       Do not create an alias
  --homepage <url>                 Formula homepage
  --desc <text>                    Formula description
  --license <id>                   Formula license (default: MIT)
  -h, --help                       Show this help

The generated formula installs:
  htree
  htree-cashu
  git-remote-htree

Examples:
  packaging/homebrew/create_tap.sh \
    --version v0.2.15 \
    --release-base-url https://upload.iris.to/<npub>/releases%2Fhashtree/v0.2.15/assets \
    --assets-dir rust/dist/hashtree-v0.2.15 \
    --output-dir dist/homebrew-htree.git
EOF
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"

VERSION=""
RELEASE_BASE_URL=""
ASSETS_DIR=""
OUTPUT_DIR=""
FORMULA_NAME="htree"
ALIAS_NAME="hashtree"
CREATE_ALIAS=1
HOMEPAGE="https://git.iris.to/#/npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/hashtree"
DESC="Hashtree daemon and CLI - content-addressed storage with P2P sync"
LICENSE_ID="MIT"

require_command() {
    local cmd="$1"
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "Missing required command: $cmd" >&2
        exit 1
    fi
}

formula_class_name() {
    local name="$1"
    awk -F'[-_]' '
        {
            for (i = 1; i <= NF; i++) {
                printf toupper(substr($i, 1, 1)) substr($i, 2)
            }
            printf "\n"
        }
    ' <<<"$name"
}

escape_ruby_string() {
    local value="$1"
    value="${value//\\/\\\\}"
    value="${value//\"/\\\"}"
    printf '%s' "$value"
}

checksum_for_target() {
    local target="$1"
    local file="${ASSETS_DIR}/hashtree-${target}.tar.gz"

    if [ ! -f "$file" ]; then
        echo "Missing release archive: $file" >&2
        exit 1
    fi

    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$file" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$file" | awk '{print $1}'
    else
        echo "Missing required command: sha256sum or shasum" >&2
        exit 1
    fi
}

asset_url_for_target() {
    local target="$1"
    local separator="?"

    if [[ "$RELEASE_BASE_URL" == *\?* ]]; then
        separator="&"
    fi

    escape_ruby_string "${RELEASE_BASE_URL}/hashtree-${target}.tar.gz${separator}v=${VERSION}"
}

write_formula() {
    local output_file="$1"
    local class_name="$2"
    local formula_version="${VERSION#v}"
    local homepage_escaped desc_escaped license_escaped
    local url_macos_arm url_macos_x86 url_linux_arm url_linux_x86
    local sha_macos_arm sha_macos_x86 sha_linux_arm sha_linux_x86

    homepage_escaped="$(escape_ruby_string "$HOMEPAGE")"
    desc_escaped="$(escape_ruby_string "$DESC")"
    license_escaped="$(escape_ruby_string "$LICENSE_ID")"
    url_macos_arm="$(asset_url_for_target "aarch64-apple-darwin")"
    url_macos_x86="$(asset_url_for_target "x86_64-apple-darwin")"
    url_linux_arm="$(asset_url_for_target "aarch64-unknown-linux-musl")"
    url_linux_x86="$(asset_url_for_target "x86_64-unknown-linux-musl")"
    sha_macos_arm="$(checksum_for_target "aarch64-apple-darwin")"
    sha_macos_x86="$(checksum_for_target "x86_64-apple-darwin")"
    sha_linux_arm="$(checksum_for_target "aarch64-unknown-linux-musl")"
    sha_linux_x86="$(checksum_for_target "x86_64-unknown-linux-musl")"

    cat >"$output_file" <<EOF
class ${class_name} < Formula
  desc "${desc_escaped}"
  homepage "${homepage_escaped}"
  version "${formula_version}"
  license "${license_escaped}"

  on_macos do
    if Hardware::CPU.arm?
      url "${url_macos_arm}"
      sha256 "${sha_macos_arm}"
    else
      url "${url_macos_x86}"
      sha256 "${sha_macos_x86}"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "${url_linux_arm}"
      sha256 "${sha_linux_arm}"
    else
      url "${url_linux_x86}"
      sha256 "${sha_linux_x86}"
    end
  end

  def install
    bin.install "htree", "htree-cashu", "git-remote-htree"
  end

  test do
    assert_match "htree", shell_output("#{bin}/htree --help")
  end
end
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version)
            VERSION="${2:-}"
            shift 2
            ;;
        --release-base-url)
            RELEASE_BASE_URL="${2:-}"
            shift 2
            ;;
        --assets-dir)
            ASSETS_DIR="${2:-}"
            shift 2
            ;;
        --checksums-dir)
            ASSETS_DIR="${2:-}"
            shift 2
            ;;
        --output-dir)
            OUTPUT_DIR="${2:-}"
            shift 2
            ;;
        --formula-name)
            FORMULA_NAME="${2:-}"
            shift 2
            ;;
        --alias-name)
            ALIAS_NAME="${2:-}"
            CREATE_ALIAS=1
            shift 2
            ;;
        --no-alias)
            CREATE_ALIAS=0
            shift
            ;;
        --homepage)
            HOMEPAGE="${2:-}"
            shift 2
            ;;
        --desc)
            DESC="${2:-}"
            shift 2
            ;;
        --license)
            LICENSE_ID="${2:-}"
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

if [ -z "$VERSION" ] || [ -z "$RELEASE_BASE_URL" ] || [ -z "$ASSETS_DIR" ] || [ -z "$OUTPUT_DIR" ]; then
    usage >&2
    exit 1
fi

if [ ! -d "$ASSETS_DIR" ]; then
    echo "Assets directory does not exist: $ASSETS_DIR" >&2
    exit 1
fi

require_command git
require_command awk

class_name="$(formula_class_name "$FORMULA_NAME")"
tmp_dir="$(mktemp -d)"
work_repo="${tmp_dir}/homebrew-${FORMULA_NAME}"

trap 'rm -rf "$tmp_dir"' EXIT

mkdir -p "${work_repo}/Formula"
write_formula "${work_repo}/Formula/${FORMULA_NAME}.rb" "$class_name"

if [ "$CREATE_ALIAS" -eq 1 ] && [ -n "$ALIAS_NAME" ] && [ "$ALIAS_NAME" != "$FORMULA_NAME" ]; then
    mkdir -p "${work_repo}/Aliases"
    ln -s "../Formula/${FORMULA_NAME}.rb" "${work_repo}/Aliases/${ALIAS_NAME}"
fi

(
    cd "$work_repo"
    git init -b master >/dev/null
    git add .
    git -c user.name='Codex' -c user.email='codex@example.com' commit -m "Add ${FORMULA_NAME} formula" >/dev/null
)

rm -rf "$OUTPUT_DIR"
git clone --bare "$work_repo" "$OUTPUT_DIR" >/dev/null
GIT_DIR="$OUTPUT_DIR" git update-server-info

cat <<EOF
Created bare tap repository:
  ${OUTPUT_DIR}

Formula:
  ${FORMULA_NAME}
EOF

if [ "$CREATE_ALIAS" -eq 1 ] && [ -n "$ALIAS_NAME" ] && [ "$ALIAS_NAME" != "$FORMULA_NAME" ]; then
    cat <<EOF
Alias:
  ${ALIAS_NAME}
EOF
fi

cat <<EOF

Next step:
  Publish ${OUTPUT_DIR} on static HTTP hosting and tap it with:
  brew tap <user>/<repo> <URL-to-${OUTPUT_DIR##*/}>
EOF
