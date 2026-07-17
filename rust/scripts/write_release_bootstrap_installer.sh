#!/bin/bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: rust/scripts/write_release_bootstrap_installer.sh --path <path> --base-url <url> [options]

Writes the top-level install.sh bootstrap used by release directories. The
script downloads the platform archive from the same release root and then
delegates to the packaged installer inside the archive.

Options:
  --asset-base-url <url>  Override the archive directory (default: BASE_URL/assets)
EOF
}

PATH_ARG=""
BASE_URL=""
ASSET_BASE_URL=""

while [ $# -gt 0 ]; do
    case "$1" in
        --path)
            PATH_ARG="${2:-}"
            shift 2
            ;;
        --base-url)
            BASE_URL="${2:-}"
            shift 2
            ;;
        --asset-base-url)
            ASSET_BASE_URL="${2:-}"
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

if [ -z "$PATH_ARG" ] || [ -z "$BASE_URL" ]; then
    usage >&2
    exit 1
fi

if [ -z "$ASSET_BASE_URL" ]; then
    ASSET_BASE_URL="${BASE_URL}/assets"
fi

CACHE_BUSTER="${BASE_URL##*/}"

mkdir -p "$(dirname "$PATH_ARG")"

cat >"$PATH_ARG" <<EOF
#!/bin/sh
set -eu

BASE_URL="${BASE_URL}"
ASSET_BASE_URL="${ASSET_BASE_URL}"
ASSET_URL_SUFFIX="?v=${CACHE_BUSTER}"

# The release root that serves this bootstrap is the trust boundary. Same-origin
# checksum or signature sidecars do not add a separate security boundary here.

log() {
    printf 'hashtree-install: %s\n' "\$*" >&2
}

die() {
    printf 'hashtree-install: error: %s\n' "\$*" >&2
    exit 1
}

tmpdir=""

on_exit() {
    rc=\$?
    if [ -n "\$tmpdir" ] && [ -d "\$tmpdir" ]; then
        rm -rf "\$tmpdir"
    fi
    if [ "\$rc" -ne 0 ]; then
        printf 'hashtree-install: install failed.\n' >&2
        printf 'hashtree-install: this error is from the installer script (install.sh),\n' >&2
        printf 'hashtree-install: not from the curl that downloaded this script.\n' >&2
    fi
}

trap on_exit EXIT HUP INT TERM

require_command() {
    if ! command -v "\$1" >/dev/null 2>&1; then
        die "missing required command: \$1"
    fi
}

detect_arch() {
    case "\$(uname -m)" in
        arm64|aarch64)
            printf '%s\n' aarch64
            ;;
        x86_64|amd64)
            printf '%s\n' x86_64
            ;;
        *)
            die "unsupported architecture: \$(uname -m)"
            ;;
    esac
}

detect_os() {
    case "\$(uname -s)" in
        Darwin)
            printf '%s\n' apple-darwin
            ;;
        Linux)
            printf '%s\n' unknown-linux-musl
            ;;
        *)
            die "unsupported operating system: \$(uname -s)"
            ;;
    esac
}

url_host() {
    printf '%s\n' "\$1" | sed \
        -e 's|^[a-zA-Z][a-zA-Z0-9+.-]*://||' \
        -e 's|/.*||' \
        -e 's|^[^@]*@||' \
        -e 's|:.*||'
}

fetch() {
    fetch_url=\$1
    fetch_out=\$2
    fetch_host=\$(url_host "\$fetch_url")
    fetch_rc=0
    fetch_http=\$(curl -fSL -o "\$fetch_out" -w '%{http_code}' "\$fetch_url") || fetch_rc=\$?

    if [ "\$fetch_rc" -eq 0 ]; then
        return 0
    fi

    case "\$fetch_rc" in
        6)
            die "could not resolve host '\$fetch_host' (fetching \$fetch_url) -- check DNS/network"
            ;;
        7)
            die "could not connect to host '\$fetch_host' (fetching \$fetch_url) -- check network"
            ;;
        28)
            die "timed out contacting '\$fetch_host' (fetching \$fetch_url)"
            ;;
        22)
            case "\$fetch_http" in
                404)
                    die "release asset not found (HTTP 404): \$fetch_url -- the version may have been removed or renamed"
                    ;;
                401|403)
                    die "access denied (HTTP \$fetch_http) fetching \$fetch_url"
                    ;;
                5*)
                    die "server error (HTTP \$fetch_http) fetching \$fetch_url -- try again later"
                    ;;
                *)
                    die "HTTP \$fetch_http fetching \$fetch_url"
                    ;;
            esac
            ;;
        *)
            die "curl failed (exit \$fetch_rc) fetching \$fetch_url"
            ;;
    esac
}

require_command curl
require_command tar
require_command mktemp
require_command uname
require_command sed

target="\$(detect_arch)-\$(detect_os)"
archive="hashtree-\${target}.tar.gz"
tmpdir=\$(mktemp -d 2>/dev/null || mktemp -d -t hashtree-install) || die "failed to create temporary directory"
[ -d "\$tmpdir" ] || die "temporary directory was not created"

url="\${ASSET_BASE_URL}/\${archive}\${ASSET_URL_SUFFIX}"
archive_path="\${tmpdir}/\${archive}"

log "downloading \${url}"
fetch "\$url" "\$archive_path"

[ -s "\$archive_path" ] || die "downloaded archive is empty or missing: \$archive_path"
tar -tzf "\$archive_path" >/dev/null 2>&1 || die "downloaded file is not a valid gzip tar archive: \$archive_path (download may be corrupt)"
tar -xzf "\$archive_path" -C "\$tmpdir" || die "failed to extract archive: \$archive_path"

packaged_dir="\${tmpdir}/hashtree"
packaged_installer="\${packaged_dir}/install.sh"

[ -d "\$packaged_dir" ] || die "expected directory 'hashtree/' not found in archive (archive layout may have changed)"
[ -f "\$packaged_installer" ] || die "packaged installer not found: hashtree/install.sh (archive layout may have changed)"
[ -x "\$packaged_installer" ] || die "packaged installer is not executable: hashtree/install.sh"

log "running packaged installer"
cd "\$packaged_dir"
./install.sh "\$@"
EOF

chmod +x "$PATH_ARG"
