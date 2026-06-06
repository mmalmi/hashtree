#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUST_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
RUN_SCRIPT="${RUST_DIR}/scripts/test_install_matrix.sh"

TMPDIR="$(mktemp -d)"
cleanup() {
    rm -rf "$TMPDIR"
}
trap cleanup EXIT

BIN_DIR="${TMPDIR}/bin"
LOG_FILE="${TMPDIR}/calls.log"
TEST_HOME="${TMPDIR}/home"
TEST_BREW_REPO="${TMPDIR}/homebrew-repo"
TEST_BREW_STATE="${TMPDIR}/brew-version"
mkdir -p "$BIN_DIR"
mkdir -p "$TEST_HOME"
mkdir -p "$TEST_BREW_REPO"

cat >"${BIN_DIR}/uname" <<'EOF'
#!/bin/bash
set -euo pipefail
case "${1:-}" in
    -s)
        printf 'Darwin\n'
        ;;
    -m)
        printf 'arm64\n'
        ;;
    *)
        printf 'Darwin\n'
        ;;
esac
EOF
chmod +x "${BIN_DIR}/uname"

cat >"${BIN_DIR}/fake-install" <<'EOF'
#!/bin/bash
set -euo pipefail
mkdir -p "${HOME}/.local/bin"
cat >"${HOME}/.local/bin/htree" <<'SCRIPT'
#!/bin/bash
set -euo pipefail
if [ "${1:-}" = "--help" ]; then
    printf 'Content-addressed filesystem\n'
    exit 0
fi
exit 0
SCRIPT
cat >"${HOME}/.local/bin/htree-cashu" <<'SCRIPT'
#!/bin/bash
set -euo pipefail
exit 0
SCRIPT
cat >"${HOME}/.local/bin/git-remote-htree" <<'SCRIPT'
#!/bin/bash
set -euo pipefail
if [ "${1:-}" != "origin" ]; then
    exit 1
fi
cat >/dev/null
printf 'fetch\npush\noption\n\n'
SCRIPT
cat >"${HOME}/.local/bin/git" <<'SCRIPT'
#!/bin/bash
set -euo pipefail
if [ "${1:-}" = "ls-remote" ]; then
    printf '0123456789abcdef0123456789abcdef01234567\tHEAD\n'
    exit 0
fi
exit 1
SCRIPT
chmod +x "${HOME}/.local/bin/htree" "${HOME}/.local/bin/htree-cashu" "${HOME}/.local/bin/git-remote-htree" "${HOME}/.local/bin/git"
EOF
chmod +x "${BIN_DIR}/fake-install"

cat >"${BIN_DIR}/git" <<'EOF'
#!/bin/bash
set -euo pipefail
if [ "${1:-}" = "-C" ]; then
    shift 2
fi
if [ "${1:-}" = "ls-remote" ]; then
    printf '0123456789abcdef0123456789abcdef01234567\tHEAD\n'
    exit 0
fi
if [ "${1:-}" = "fetch" ] || [ "${1:-}" = "reset" ]; then
    exit 0
fi
exit 1
EOF
chmod +x "${BIN_DIR}/git"

cat >"${BIN_DIR}/docker" <<'EOF'
#!/bin/bash
set -euo pipefail
printf 'docker:%s\n' "$*" >>"${TEST_LOG_FILE}"
platform=""
previous=""
for arg in "$@"; do
    if [ "$previous" = "--platform" ]; then
        platform="$arg"
        previous=""
        continue
    fi
    previous="$arg"
done

case "$*" in
    *"alpine:3.22 true"*)
        exit 0
        ;;
esac

case "$platform" in
    linux/arm64)
        exit 0
        ;;
    linux/amd64)
        printf 'Store error: Function not implemented (os error 38)\n' >&2
        exit 1
        ;;
esac

exit 1
EOF
chmod +x "${BIN_DIR}/docker"

cat >"${BIN_DIR}/brew" <<'EOF'
#!/bin/bash
set -euo pipefail
printf 'brew:%s\n' "$*" >>"${TEST_LOG_FILE}"
case "${1:-}" in
    --repo)
        printf '%s\n' "${TEST_BREW_REPO}"
        exit 0
        ;;
    tap)
        mkdir -p "${TEST_BREW_REPO}/Formula"
        cat >"${TEST_BREW_REPO}/Formula/htree.rb" <<'FORMULA'
class Htree < Formula
  version "0.2.32"
end
FORMULA
        if [ $# -eq 1 ]; then
            printf 'sirius/hashtree\n'
            exit 0
        fi
        exit 0
        ;;
    list)
        if [ -f "${TEST_BREW_STATE}" ]; then
            printf 'htree %s\n' "$(cat "${TEST_BREW_STATE}")"
            exit 0
        fi
        exit 1
        ;;
    install|reinstall)
        version="$(sed -n 's/^  version "\([^"]*\)".*/\1/p' "${TEST_BREW_REPO}/Formula/htree.rb" | head -n1)"
        printf '%s\n' "${version:-0.2.32}" >"${TEST_BREW_STATE}"
        exit 0
        ;;
    test|info|uninstall)
        exit 0
        ;;
    untap)
        rm -rf "${TEST_BREW_REPO}"
        rm -f "${TEST_BREW_STATE}"
        exit 0
        ;;
esac
exit 0
EOF
chmod +x "${BIN_DIR}/brew"

cat >"${BIN_DIR}/prlctl" <<'EOF'
#!/bin/bash
set -euo pipefail
printf 'prlctl:%s\n' "$*" >>"${TEST_LOG_FILE}"
case "${1:-}" in
    list)
        printf 'UUID                                    STATUS       IP_ADDR         NAME\n'
        printf '{00000000-0000-0000-0000-000000000000}  running      -               Windows 11\n'
        ;;
    exec)
        previous=""
        for arg in "$@"; do
            if [ "$previous" = "-File" ]; then
                host_path="${HOME}${arg#C:\\Mac\\Home}"
                host_path="${host_path//\\//}"
                script_dir="$(dirname "$host_path")"
                mkdir -p "$script_dir"
                printf 'download-ok\nhelper-capabilities-ok\n' >"${script_dir}/guest.log"
                printf 'PASS\ndownloaded the Windows zip and verified htree.exe plus git-remote-htree.exe\n' >"${script_dir}/result.txt"
                break
            fi
            previous="$arg"
        done
        exit 0
        ;;
esac
EOF
chmod +x "${BIN_DIR}/prlctl"

OUTPUT_FILE="${TMPDIR}/matrix.out"
set +e
PATH="${BIN_DIR}:/usr/bin:/bin" HOME="${TEST_HOME}" TEST_LOG_FILE="${LOG_FILE}" TEST_BREW_REPO="${TEST_BREW_REPO}" TEST_BREW_STATE="${TEST_BREW_STATE}" \
    "${RUN_SCRIPT}" \
    --install-cmd "${BIN_DIR}/fake-install" \
    --windows-zip-url "https://example.test/hashtree.zip" \
    --brew-tap-name "sirius/hashtree" \
    --brew-tap-url "https://example.test/homebrew-hashtree.git" \
    --platforms "host,docker-arm64,docker-amd64,windows,brew" \
    >"${OUTPUT_FILE}" 2>&1
status=$?
set -e

test "$status" -eq 1
grep -F "PASS     host-darwin-arm64" "${OUTPUT_FILE}" >/dev/null
grep -F "PASS     docker-linux-arm64" "${OUTPUT_FILE}" >/dev/null
grep -F "FAIL     docker-linux-amd64" "${OUTPUT_FILE}" >/dev/null
grep -F "PASS     windows-vm-x86_64" "${OUTPUT_FILE}" >/dev/null
grep -F "PASS     homebrew-host" "${OUTPUT_FILE}" >/dev/null
grep -F "Summary: 4 passed, 1 failed, 0 skipped" "${OUTPUT_FILE}" >/dev/null

grep -F "docker:run --rm --platform linux/arm64 alpine:3.22 true" "${LOG_FILE}" >/dev/null
grep -F "docker:run --rm --platform linux/amd64 alpine:3.22 true" "${LOG_FILE}" >/dev/null
grep -F "brew:--repo sirius/hashtree" "${LOG_FILE}" >/dev/null
grep -F "brew:test htree" "${LOG_FILE}" >/dev/null
grep -F "prlctl:list -a" "${LOG_FILE}" >/dev/null
grep -F "prlctl:exec Windows 11 --current-user cmd.exe /c start" "${LOG_FILE}" >/dev/null

echo "test_install_matrix_script.sh passed"
