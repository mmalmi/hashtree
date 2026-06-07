#!/bin/bash
set -uo pipefail

usage() {
    cat <<'EOF'
Usage: rust/scripts/test_install_matrix.sh [options]

Smoke-test the README-advertised install flows on every platform this machine
can reach: native host, Docker Linux targets, a running Windows VM, and
Homebrew on the host when available.

By default the script extracts the canonical install command and Homebrew tap
from README.md. It prints a per-platform PASS/FAIL/SKIP summary and exits nonzero
if any attempted platform fails.

Options:
  --readme <path>                 README to inspect (default: repo README.md)
  --install-cmd <command>         Override the install command to test
  --test-remote <htree-url>       Remote used for git/helper smoke checks
  --platforms <csv>               Subset: host,docker-arm64,docker-amd64,windows,brew
  --docker-bin <path>             Docker binary to use (default: docker)
  --docker-image <image>          Debian image used for Linux smoke tests
  --timeout-seconds <seconds>     Timeout for Docker and Windows smoke runs (default: 180)
  --windows-zip-url <url>         Override the Windows zip asset URL
  --windows-vm-name <name>        Override the Parallels Windows VM name
  --brew-tap-name <name>          Override the Homebrew tap name
  --brew-tap-url <url>            Override the Homebrew tap URL
  --brew-formula <name>           Override the Homebrew formula name (default: htree)
  --keep-temp                     Keep temporary work directories for debugging
  -h, --help                      Show this help

Examples:
  rust/scripts/test_install_matrix.sh
  rust/scripts/test_install_matrix.sh --platforms host,docker-arm64
  rust/scripts/test_install_matrix.sh --install-cmd 'curl -fsSL https://example/install.sh | sh'
EOF
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUST_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
REPO_ROOT="$(cd "${RUST_DIR}/.." && pwd)"

README_PATH="${REPO_ROOT}/README.md"
INSTALL_CMD=""
TEST_REMOTE="htree://npub1xdhnr9mrv47kkrn95k6cwecearydeh8e895990n3acntwvmgk2dsdeeycm/hashtree"
PLATFORMS_CSV="host,docker-arm64,docker-amd64,windows,brew"
DOCKER_BIN="${DOCKER_BIN:-docker}"
DOCKER_IMAGE="${DOCKER_IMAGE:-debian:bookworm-slim}"
COMMAND_TIMEOUT_SECONDS="${COMMAND_TIMEOUT_SECONDS:-180}"
WINDOWS_ZIP_URL=""
WINDOWS_VM_NAME=""
BREW_TAP_NAME=""
BREW_TAP_URL=""
BREW_FORMULA="htree"
KEEP_TEMP=0

RESULT_PLATFORMS=()
RESULT_STATUSES=()
RESULT_NOTES=()
EXTRA_TEMP_PATHS=()

WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/hashtree-install-matrix.XXXXXX")"
LOG_DIR="${WORK_DIR}/logs"
mkdir -p "$LOG_DIR"

cleanup() {
    local path
    for path in "${EXTRA_TEMP_PATHS[@]:-}"; do
        if [ "$KEEP_TEMP" -eq 0 ] && [ -n "$path" ] && [ -e "$path" ]; then
            rm -rf "$path"
        fi
    done
    if [ "$KEEP_TEMP" -eq 0 ] && [ -n "${WORK_DIR:-}" ] && [ -d "$WORK_DIR" ]; then
        rm -rf "$WORK_DIR"
    fi
}
trap cleanup EXIT

while [ $# -gt 0 ]; do
    case "$1" in
        --readme)
            README_PATH="${2:-}"
            shift 2
            ;;
        --install-cmd)
            INSTALL_CMD="${2:-}"
            shift 2
            ;;
        --test-remote)
            TEST_REMOTE="${2:-}"
            shift 2
            ;;
        --platforms)
            PLATFORMS_CSV="${2:-}"
            shift 2
            ;;
        --docker-bin)
            DOCKER_BIN="${2:-}"
            shift 2
            ;;
        --docker-image)
            DOCKER_IMAGE="${2:-}"
            shift 2
            ;;
        --timeout-seconds)
            COMMAND_TIMEOUT_SECONDS="${2:-}"
            shift 2
            ;;
        --windows-zip-url)
            WINDOWS_ZIP_URL="${2:-}"
            shift 2
            ;;
        --windows-vm-name)
            WINDOWS_VM_NAME="${2:-}"
            shift 2
            ;;
        --brew-tap-name)
            BREW_TAP_NAME="${2:-}"
            shift 2
            ;;
        --brew-tap-url)
            BREW_TAP_URL="${2:-}"
            shift 2
            ;;
        --brew-formula)
            BREW_FORMULA="${2:-}"
            shift 2
            ;;
        --keep-temp)
            KEEP_TEMP=1
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

trim() {
    local value="$1"
    value="${value#"${value%%[![:space:]]*}"}"
    value="${value%"${value##*[![:space:]]}"}"
    printf '%s\n' "$value"
}

truncate_note() {
    local note
    note="$(trim "$1")"
    note="${note//$'\r'/ }"
    note="${note//$'\n'/; }"
    while [[ "$note" == *"  "* ]]; do
        note="${note//  / }"
    done
    if [ ${#note} -gt 180 ]; then
        note="${note:0:177}..."
    fi
    printf '%s\n' "$note"
}

record_result() {
    local platform="$1"
    local status="$2"
    local note="$3"
    RESULT_PLATFORMS+=("$platform")
    RESULT_STATUSES+=("$status")
    RESULT_NOTES+=("$(truncate_note "$note")")
}

platform_requested() {
    case ",${PLATFORMS_CSV}," in
        *",$1,"*)
            return 0
            ;;
        *)
            return 1
            ;;
    esac
}

platform_log_path() {
    local label="$1"
    label="${label//[^[:alnum:]._-]/_}"
    printf '%s/%s.log\n' "$LOG_DIR" "$label"
}

failure_note_from_log() {
    local log_path="$1"
    local tail_text
    if [ ! -s "$log_path" ]; then
        printf 'see %s (no output captured)\n' "$log_path"
        return 0
    fi
    tail_text="$(tail -n 8 "$log_path" 2>/dev/null || true)"
    printf 'see %s; tail: %s\n' "$log_path" "$tail_text"
}

run_with_timeout() {
    local timeout_seconds="$1"
    local log_path="$2"
    shift 2

    if [ "${timeout_seconds}" -le 0 ] || ! command -v python3 >/dev/null 2>&1; then
        "$@" >"$log_path" 2>&1
        return $?
    fi

    python3 - "$timeout_seconds" "$log_path" "$@" <<'PY'
import subprocess
import sys

timeout = int(sys.argv[1])
log_path = sys.argv[2]
command = sys.argv[3:]

with open(log_path, "wb") as log_file:
    try:
        proc = subprocess.Popen(command, stdout=log_file, stderr=subprocess.STDOUT)
        raise SystemExit(proc.wait(timeout=timeout))
    except subprocess.TimeoutExpired:
        proc.kill()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass
        log_file.write(f"\nTimed out after {timeout}s\n".encode("utf-8"))
        raise SystemExit(124)
PY
}

extract_install_cmd_from_readme() {
    grep -F 'curl -fsSL https://upload.iris.to/' "$README_PATH" | grep 'install.sh' | head -n1
}

extract_brew_tap_from_readme() {
    grep -F 'brew tap ' "$README_PATH" | grep 'homebrew-hashtree.git' | head -n1
}

extract_install_url_from_cmd() {
    local cmd="$1"
    if [[ "$cmd" =~ (https?://[^[:space:]\"\'\|]+install\.sh) ]]; then
        printf '%s\n' "${BASH_REMATCH[1]}"
        return 0
    fi
    return 1
}

derive_windows_zip_url() {
    local install_url install_script base_url
    if [ -n "$WINDOWS_ZIP_URL" ]; then
        printf '%s\n' "$WINDOWS_ZIP_URL"
        return 0
    fi
    install_url="$(extract_install_url_from_cmd "$INSTALL_CMD")" || return 1
    install_script="$(curl -fsSL "$install_url")" || return 1
    base_url="$(printf '%s\n' "$install_script" | sed -n 's/^BASE_URL="\([^"]*\)".*/\1/p' | head -n1)"
    if [ -z "$base_url" ]; then
        return 1
    fi
    printf '%s/assets/hashtree-x86_64-pc-windows-msvc.zip\n' "$base_url"
}

auto_detect_windows_vm_name() {
    local listing line status name count=0 match=""
    listing="$(prlctl list -a 2>/dev/null || true)"
    while IFS= read -r line; do
        case "$line" in
            \{*\}\ *)
                status="$(printf '%s\n' "$line" | awk '{print tolower($2)}')"
                name="$(printf '%s\n' "$line" | sed -E 's/^\{[^}]+\}[[:space:]]+(running|suspended)[[:space:]]+[^[:space:]]+[[:space:]]+//I')"
                if [ "$name" = "$line" ]; then
                    name="$(printf '%s\n' "$line" | sed -E 's/^\{[^}]+\}[[:space:]]+(running|suspended)[[:space:]]+//I')"
                fi
                if { [ "$status" = "running" ] || [ "$status" = "suspended" ]; } &&
                    printf '%s\n' "$name" | grep -qi 'windows'
                then
                    count=$((count + 1))
                    match="$name"
                fi
                ;;
        esac
    done <<EOF
$listing
EOF
    if [ "$count" -eq 1 ] && [ -n "$match" ]; then
        printf '%s\n' "$match"
        return 0
    fi
    return 1
}

shared_windows_path() {
    if ! command -v python3 >/dev/null 2>&1; then
        return 1
    fi
    python3 - "$1" "$HOME" <<'PY'
import pathlib
import sys

path = pathlib.Path(sys.argv[1]).expanduser().resolve()
home = pathlib.Path(sys.argv[2]).expanduser().resolve()

try:
    rel = path.relative_to(home)
except ValueError:
    raise SystemExit(1)

if str(rel) == '.':
    print(r'C:\Mac\Home')
else:
    print(r'C:\Mac\Home' + '\\' + str(rel).replace('/', '\\'))
PY
}

docker_platform_available() {
    "$DOCKER_BIN" run --rm --platform "$1" alpine:3.22 true >/dev/null 2>&1
}

run_unix_install_smoke() {
    local install_cmd="$1"
    local test_remote="$2"
    local home_dir="$3"
    local system_path="/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"

    mkdir -p "$home_dir"
    env HOME="$home_dir" PATH="${system_path}:${PATH:-}" /bin/bash -c "$install_cmd"
    env HOME="$home_dir" PATH="$home_dir/.local/bin:${system_path}:${PATH:-}" \
        /bin/bash -c '
            set -euo pipefail
            command -v htree >/dev/null || { echo "htree not found on PATH=$PATH" >&2; exit 1; }
            command -v htree-cashu >/dev/null || { echo "htree-cashu not found on PATH=$PATH" >&2; exit 1; }
            command -v git-remote-htree >/dev/null || { echo "git-remote-htree not found on PATH=$PATH" >&2; exit 1; }
            htree --help >/dev/null || { echo "htree --help failed" >&2; exit 1; }
            helper_out="$(printf "capabilities\n" | git-remote-htree origin "'"$test_remote"'")" || {
                echo "git-remote-htree capabilities command failed" >&2
                exit 1
            }
            printf "%s\n" "$helper_out" | grep -Fx "fetch" >/dev/null || { echo "git-remote-htree missing fetch capability" >&2; exit 1; }
            printf "%s\n" "$helper_out" | grep -Fx "push" >/dev/null || { echo "git-remote-htree missing push capability" >&2; exit 1; }
            printf "%s\n" "$helper_out" | grep -Fx "option" >/dev/null || { echo "git-remote-htree missing option capability" >&2; exit 1; }
            git ls-remote "'"$test_remote"'" >/dev/null || { echo "git ls-remote failed for '"$test_remote"'" >&2; exit 1; }
        '
}

run_host_smoke() {
    local host_os host_arch label home_dir log_path
    host_os="$(uname -s 2>/dev/null | tr '[:upper:]' '[:lower:]')"
    host_arch="$(uname -m 2>/dev/null | tr '[:upper:]' '[:lower:]')"
    label="host-${host_os}-${host_arch}"

    case "$host_os" in
        darwin|linux)
            ;;
        *)
            record_result "$label" "SKIP" "shell bootstrap is only supported on macOS/Linux hosts"
            return 0
            ;;
    esac

    home_dir="${WORK_DIR}/${label}-home"
    log_path="$(platform_log_path "$label")"
    if run_unix_install_smoke "$INSTALL_CMD" "$TEST_REMOTE" "$home_dir" >"$log_path" 2>&1; then
        record_result "$label" "PASS" "install command, htree --help, helper capabilities, and git ls-remote succeeded"
    else
        record_result "$label" "FAIL" "$(failure_note_from_log "$log_path")"
    fi
}

run_docker_smoke() {
    local platform="$1"
    local label="$2"
    local log_path

    if ! command -v "$DOCKER_BIN" >/dev/null 2>&1; then
        record_result "$label" "SKIP" "docker not available"
        return 0
    fi

    if ! docker_platform_available "$platform"; then
        record_result "$label" "SKIP" "${platform} is not runnable via docker on this machine"
        return 0
    fi

    log_path="$(platform_log_path "$label")"
    if run_with_timeout "$COMMAND_TIMEOUT_SECONDS" "$log_path" \
        env INSTALL_CMD="$INSTALL_CMD" TEST_REMOTE="$TEST_REMOTE" \
            "$DOCKER_BIN" run --rm --platform "$platform" \
            -e INSTALL_CMD \
            -e TEST_REMOTE \
            "$DOCKER_IMAGE" \
            sh -lc '
                set -eu
                export DEBIAN_FRONTEND=noninteractive
                apt-get update >/dev/null
                apt-get install -y --no-install-recommends bash curl ca-certificates git >/dev/null
                export HOME=/tmp/hashtree-home
                mkdir -p "$HOME"
                /bin/bash -c "$INSTALL_CMD"
                PATH="$HOME/.local/bin:/usr/local/bin:/usr/bin:/bin" /bin/bash -c '"'"'
                    set -euo pipefail
                    command -v htree >/dev/null || { echo "htree not found on PATH=$PATH" >&2; exit 1; }
                    command -v htree-cashu >/dev/null || { echo "htree-cashu not found on PATH=$PATH" >&2; exit 1; }
                    command -v git-remote-htree >/dev/null || { echo "git-remote-htree not found on PATH=$PATH" >&2; exit 1; }
                    htree --help >/dev/null || { echo "htree --help failed" >&2; exit 1; }
                    helper_out="$(printf "capabilities\n" | git-remote-htree origin "$TEST_REMOTE")" || {
                        echo "git-remote-htree capabilities command failed" >&2
                        exit 1
                    }
                    printf "%s\n" "$helper_out" | grep -Fx "fetch" >/dev/null || { echo "git-remote-htree missing fetch capability" >&2; exit 1; }
                    printf "%s\n" "$helper_out" | grep -Fx "push" >/dev/null || { echo "git-remote-htree missing push capability" >&2; exit 1; }
                    printf "%s\n" "$helper_out" | grep -Fx "option" >/dev/null || { echo "git-remote-htree missing option capability" >&2; exit 1; }
                    git ls-remote "$TEST_REMOTE" >/dev/null || { echo "git ls-remote failed for $TEST_REMOTE" >&2; exit 1; }
                '"'"'
            '; then
        record_result "$label" "PASS" "install command, htree --help, helper capabilities, and git ls-remote succeeded"
    else
        record_result "$label" "FAIL" "$(failure_note_from_log "$log_path")"
    fi
}

powershell_escape() {
    printf '%s' "$1" | sed "s/'/''/g"
}

run_windows_smoke() {
    local vm_name zip_url label log_path shared_dir shared_script_path shared_log_path shared_result_path windows_script_path
    local windows_log_path windows_result_path start_log_path launch_rc status_line detail_line elapsed poll_interval
    label="windows-vm-x86_64"

    if [ "$(uname -s 2>/dev/null)" != "Darwin" ]; then
        record_result "$label" "SKIP" "Parallels Windows VM smoke only runs from macOS"
        return 0
    fi
    if ! command -v prlctl >/dev/null 2>&1; then
        record_result "$label" "SKIP" "prlctl not available"
        return 0
    fi
    if ! command -v python3 >/dev/null 2>&1; then
        record_result "$label" "SKIP" "python3 not available for PowerShell encoding"
        return 0
    fi

    vm_name="${WINDOWS_VM_NAME:-}"
    if [ -z "$vm_name" ]; then
        vm_name="$(auto_detect_windows_vm_name || true)"
    fi
    if [ -z "$vm_name" ]; then
        record_result "$label" "SKIP" "no unique running Windows VM detected"
        return 0
    fi

    zip_url="$(derive_windows_zip_url || true)"
    if [ -z "$zip_url" ]; then
        record_result "$label" "FAIL" "could not determine the Windows release zip URL"
        return 0
    fi

    mkdir -p "${HOME}/tmp"
    shared_dir="$(mktemp -d "${HOME}/tmp/hashtree-install-matrix-windows.XXXXXX")"
    EXTRA_TEMP_PATHS+=("$shared_dir")
    windows_script_path="${shared_dir}/run.ps1"
    windows_log_path="${shared_dir}/guest.log"
    windows_result_path="${shared_dir}/result.txt"
    shared_script_path="$(shared_windows_path "$windows_script_path" || true)"
    shared_log_path="$(shared_windows_path "$windows_log_path" || true)"
    shared_result_path="$(shared_windows_path "$windows_result_path" || true)"
    log_path="$(platform_log_path "$label")"
    start_log_path="$(platform_log_path "${label}-launch")"

    if [ -z "$shared_script_path" ] || [ -z "$shared_log_path" ] || [ -z "$shared_result_path" ]; then
        record_result "$label" "FAIL" "could not map a shared host temp directory into Parallels"
        return 0
    fi

    cat >"$windows_script_path" <<EOF
\$ErrorActionPreference = 'Stop'
\$ProgressPreference = 'SilentlyContinue'
\$remote = '$(powershell_escape "$TEST_REMOTE")'
\$zipUrl = '$(powershell_escape "$zip_url")'
\$logPath = '$(powershell_escape "$shared_log_path")'
\$resultPath = '$(powershell_escape "$shared_result_path")'
\$status = 'FAIL'
\$detail = ''

function Write-GuestLog {
  param([string]\$Message)
  Add-Content -Path \$logPath -Value \$Message
}

New-Item -ItemType Directory -Path (Split-Path -Parent \$logPath) -Force | Out-Null
Set-Content -Path \$logPath -Value ''

\$work = Join-Path \$env:TEMP ('hashtree-install-test-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path \$work | Out-Null
try {
  \$zipPath = Join-Path \$work 'hashtree.zip'
  Write-GuestLog "download \$zipUrl"
  & curl.exe -fsSL \$zipUrl -o \$zipPath
  if (\$LASTEXITCODE -ne 0) { throw "curl.exe failed with exit code \$LASTEXITCODE" }
  Write-GuestLog 'download-ok'
  tar.exe -xf \$zipPath -C \$work
  if (\$LASTEXITCODE -ne 0) { throw "tar.exe failed with exit code \$LASTEXITCODE" }
  Write-GuestLog 'expand-ok'
  \$htree = Get-ChildItem -Path \$work -Recurse -Filter 'htree.exe' | Select-Object -First 1
  \$helper = Get-ChildItem -Path \$work -Recurse -Filter 'git-remote-htree.exe' | Select-Object -First 1
  if (-not \$htree) { throw 'htree.exe not found in extracted archive' }
  if (-not \$helper) { throw 'git-remote-htree.exe not found in extracted archive' }
  & \$htree.FullName --help | Out-Null
  Write-GuestLog 'htree-help-ok'
  \$cap = 'capabilities' | & \$helper.FullName origin \$remote
  if (\$cap -notcontains 'fetch' -or \$cap -notcontains 'push' -or \$cap -notcontains 'option') {
    throw 'git-remote-htree.exe did not advertise fetch/push/option'
  }
  Write-GuestLog 'helper-capabilities-ok'
  \$status = 'PASS'
  \$detail = 'downloaded the Windows zip and verified htree.exe plus git-remote-htree.exe'
}
catch {
  \$detail = \$_.Exception.Message
  Write-GuestLog \$detail
}
finally {
  Set-Content -Path \$resultPath -Value (\$status + [Environment]::NewLine + \$detail + [Environment]::NewLine)
  Remove-Item -Path \$work -Recurse -Force -ErrorAction SilentlyContinue
}
EOF

    if ! prlctl exec "$vm_name" --current-user cmd.exe /c start "" /b powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File "$shared_script_path" >"$start_log_path" 2>&1; then
        record_result "$label" "FAIL" "$(failure_note_from_log "$start_log_path")"
        return 0
    fi

    elapsed=0
    poll_interval=1
    while [ ! -f "$windows_result_path" ] && [ "$elapsed" -lt "$COMMAND_TIMEOUT_SECONDS" ]; do
        sleep "$poll_interval"
        elapsed=$((elapsed + poll_interval))
    done

    if [ ! -f "$windows_result_path" ]; then
        cp "$start_log_path" "$log_path" 2>/dev/null || true
        if [ -f "$windows_log_path" ]; then
            {
                printf '\n[guest log]\n'
                cat "$windows_log_path"
            } >>"$log_path"
        else
            printf '\nTimed out after %ss waiting for %s\n' "$COMMAND_TIMEOUT_SECONDS" "$windows_result_path" >>"$log_path"
        fi
        record_result "$label" "FAIL" "$(failure_note_from_log "$log_path")"
        return 0
    fi

    cp "$start_log_path" "$log_path" 2>/dev/null || true
    if [ -f "$windows_log_path" ]; then
        {
            printf '\n[guest log]\n'
            cat "$windows_log_path"
        } >>"$log_path"
    fi

    status_line="$(sed -n '1p' "$windows_result_path" | tr -d '\r')"
    detail_line="$(sed -n '2p' "$windows_result_path" | tr -d '\r')"
    if [ "$status_line" = "PASS" ]; then
        record_result "$label" "PASS" "${detail_line:-downloaded the Windows zip and verified htree.exe plus git-remote-htree.exe}"
    else
        if [ -n "$detail_line" ]; then
            printf '\n[guest result]\n%s\n' "$detail_line" >>"$log_path"
        fi
        record_result "$label" "FAIL" "$(failure_note_from_log "$log_path")"
    fi
}

run_brew_smoke() {
    local label="homebrew-host"
    local had_tap=0 had_formula=0 output log_path tap_repo formula_path formula_version installed_version

    if ! command -v brew >/dev/null 2>&1; then
        record_result "$label" "SKIP" "brew not available"
        return 0
    fi

    if [ -z "$BREW_TAP_NAME" ] || [ -z "$BREW_TAP_URL" ]; then
        record_result "$label" "FAIL" "could not determine the Homebrew tap from README.md"
        return 0
    fi

    log_path="$(platform_log_path "$label")"

    if output="$(
        HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1 \
            brew tap | grep -Fx "$BREW_TAP_NAME" >/dev/null
    2>&1)"; then
        had_tap=1
    else
        had_tap=0
    fi

    if [ "$had_tap" -eq 0 ]; then
        if ! HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1 brew tap "$BREW_TAP_NAME" "$BREW_TAP_URL" >"$log_path" 2>&1; then
            record_result "$label" "FAIL" "$(failure_note_from_log "$log_path")"
            return 0
        fi
    fi

    tap_repo="$(HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1 brew --repo "$BREW_TAP_NAME" 2>>"$log_path" || true)"
    if [ -z "$tap_repo" ] || [ ! -d "$tap_repo" ]; then
        record_result "$label" "FAIL" "$(failure_note_from_log "$log_path")"
        return 0
    fi
    if ! git -C "$tap_repo" fetch origin >>"$log_path" 2>&1; then
        record_result "$label" "FAIL" "$(failure_note_from_log "$log_path")"
        return 0
    fi
    if ! git -C "$tap_repo" reset --hard origin/master >>"$log_path" 2>&1; then
        record_result "$label" "FAIL" "$(failure_note_from_log "$log_path")"
        return 0
    fi

    formula_path="${tap_repo}/Formula/${BREW_FORMULA}.rb"
    if [ ! -f "$formula_path" ]; then
        record_result "$label" "FAIL" "missing formula at ${formula_path}"
        return 0
    fi
    formula_version="$(sed -n 's/^  version "\([^"]*\)".*/\1/p' "$formula_path" | head -n1)"
    if [ -z "$formula_version" ]; then
        record_result "$label" "FAIL" "could not read ${BREW_FORMULA} version from ${formula_path}"
        return 0
    fi
    installed_version=""

    if output="$(
        HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1 brew list --versions "$BREW_FORMULA"
    2>&1)" && [ -n "$(trim "$output")" ]; then
        had_formula=1
        installed_version="$(printf '%s\n' "$output" | awk 'NR==1 {print $2}')"
    else
        had_formula=0
        if ! HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1 brew install "$BREW_FORMULA" >"$log_path" 2>&1; then
            if [ "$had_tap" -eq 0 ]; then
                HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1 brew untap "$BREW_TAP_NAME" >/dev/null 2>&1 || true
            fi
            record_result "$label" "FAIL" "$(failure_note_from_log "$log_path")"
            return 0
        fi
    fi

    if [ "$had_formula" -eq 1 ] && [ -n "$installed_version" ] && [ "$installed_version" != "$formula_version" ]; then
        if ! HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1 brew reinstall "$BREW_FORMULA" >"$log_path" 2>&1; then
            record_result "$label" "FAIL" "$(failure_note_from_log "$log_path")"
            return 0
        fi
    fi

    if (
        HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1 brew test "$BREW_FORMULA" &&
            HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1 brew info "$BREW_FORMULA" >/dev/null &&
            HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1 brew info hashtree >/dev/null
    ) >"$log_path" 2>&1; then
        record_result "$label" "PASS" "brew install/test/info succeeded for ${BREW_FORMULA} ${formula_version}"
    else
        record_result "$label" "FAIL" "$(failure_note_from_log "$log_path")"
    fi

    if [ "$had_formula" -eq 0 ]; then
        HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1 brew uninstall "$BREW_FORMULA" >/dev/null 2>&1 || true
    fi
    if [ "$had_tap" -eq 0 ]; then
        HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1 brew untap "$BREW_TAP_NAME" >/dev/null 2>&1 || true
    fi
}

if [ -z "$INSTALL_CMD" ]; then
    INSTALL_CMD="$(extract_install_cmd_from_readme)"
fi
if [ -z "$INSTALL_CMD" ]; then
    echo "Failed to extract the canonical install command from ${README_PATH}" >&2
    exit 1
fi

if [ -z "$BREW_TAP_NAME" ] || [ -z "$BREW_TAP_URL" ]; then
    brew_tap_line="$(extract_brew_tap_from_readme)"
    if [[ "$brew_tap_line" =~ brew[[:space:]]+tap[[:space:]]+([^[:space:]]+)[[:space:]]+(https?://[^[:space:]]+) ]]; then
        [ -n "$BREW_TAP_NAME" ] || BREW_TAP_NAME="${BASH_REMATCH[1]}"
        [ -n "$BREW_TAP_URL" ] || BREW_TAP_URL="${BASH_REMATCH[2]}"
    fi
fi

if platform_requested host; then
    run_host_smoke
fi
if platform_requested docker-arm64; then
    run_docker_smoke "linux/arm64" "docker-linux-arm64"
fi
if platform_requested docker-amd64; then
    run_docker_smoke "linux/amd64" "docker-linux-amd64"
fi
if platform_requested windows; then
    run_windows_smoke
fi
if platform_requested brew; then
    run_brew_smoke
fi

printf '%-8s %-24s %s\n' "STATUS" "PLATFORM" "NOTE"
printf '%-8s %-24s %s\n' "------" "--------" "----"

pass_count=0
fail_count=0
skip_count=0

for i in "${!RESULT_PLATFORMS[@]}"; do
    printf '%-8s %-24s %s\n' "${RESULT_STATUSES[$i]}" "${RESULT_PLATFORMS[$i]}" "${RESULT_NOTES[$i]}"
    case "${RESULT_STATUSES[$i]}" in
        PASS)
            pass_count=$((pass_count + 1))
            ;;
        FAIL)
            fail_count=$((fail_count + 1))
            ;;
        SKIP)
            skip_count=$((skip_count + 1))
            ;;
    esac
done

printf '\nSummary: %d passed, %d failed, %d skipped\n' "$pass_count" "$fail_count" "$skip_count"
if [ "$KEEP_TEMP" -eq 1 ]; then
    printf 'Work dir: %s\n' "$WORK_DIR"
fi

if [ "$fail_count" -gt 0 ]; then
    exit 1
fi
exit 0
