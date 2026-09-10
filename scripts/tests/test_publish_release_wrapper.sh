#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"
PUBLISH_SCRIPT="${ROOT_DIR}/publish_release.sh"

TMP_DIR="$(mktemp -d)"
cleanup() {
    rm -rf "$TMP_DIR"
}
trap cleanup EXIT

TEST_REPO="${TMP_DIR}/repo"
LOG_DIR="${TMP_DIR}/logs"
BIN_DIR="${TMP_DIR}/bin"

mkdir -p "${TEST_REPO}/rust/scripts" "${TEST_REPO}/scripts" "${LOG_DIR}" "${BIN_DIR}"

cp "$PUBLISH_SCRIPT" "${TEST_REPO}/publish_release.sh"
cp "${ROOT_DIR}/scripts/check-mesh-release.py" "${TEST_REPO}/scripts/check-mesh-release.py"
chmod +x "${TEST_REPO}/publish_release.sh"

cat >"${TEST_REPO}/rust/scripts/release_to_htree.sh" <<'EOF'
#!/bin/bash
set -euo pipefail

printf '%q ' "$@" >>"${TEST_LOG_DIR}/release_to_htree.log"
printf '\n' >>"${TEST_LOG_DIR}/release_to_htree.log"

stage_dir=""
version=""
while [ $# -gt 0 ]; do
    case "$1" in
        --release-stage-dir)
            stage_dir="${2:-}"
            shift 2
            ;;
        --version)
            version="${2:-}"
            shift 2
            ;;
        *)
            shift
            ;;
    esac
done

if [ -z "$stage_dir" ] || [ -z "$version" ]; then
    echo "missing required stage dir or version" >&2
    exit 1
fi

mkdir -p "${stage_dir}/assets"
printf '{ "tag": "%s" }\n' "$version" >"${stage_dir}/release.json"
printf 'notes for %s\n' "$version" >"${stage_dir}/notes.md"
printf '#!/bin/sh\necho install\n' >"${stage_dir}/install.sh"
chmod +x "${stage_dir}/install.sh"
printf 'cli archive\n' >"${stage_dir}/assets/hashtree-aarch64-apple-darwin.tar.gz"
EOF
chmod +x "${TEST_REPO}/rust/scripts/release_to_htree.sh"

cat >"${TEST_REPO}/scripts/release-gate.sh" <<'EOF'
#!/bin/bash
set -euo pipefail
printf 'release gate invoked\n' >"${TEST_LOG_DIR}/release-gate.log"
EOF
chmod +x "${TEST_REPO}/scripts/release-gate.sh"

cat >"${BIN_DIR}/gh" <<'EOF'
#!/bin/bash
set -euo pipefail

printf '%q ' "$@" >>"${TEST_LOG_DIR}/gh.log"
printf '\n' >>"${TEST_LOG_DIR}/gh.log"

if [ "${1:-}" = "auth" ] && [ "${2:-}" = "status" ]; then
    exit 0
fi

if [ "${1:-}" = "release" ] && [ "${2:-}" = "view" ]; then
    [ "${TEST_RELEASE_EXISTS:-0}" -eq 1 ]
    exit $?
fi

if [ "${1:-}" = "release" ] && [ "${2:-}" = "upload" ] && [ "${TEST_UPLOAD_FAILURE:-0}" -eq 1 ]; then
    exit 37
fi

exit 0
EOF
chmod +x "${BIN_DIR}/gh"

git init "${TEST_REPO}" >/dev/null
git -C "${TEST_REPO}" remote add github git@github.com:mmalmi/hashtree.git
git -C "${TEST_REPO}" config user.email test@example.invalid
git -C "${TEST_REPO}" config user.name "Release Test"
git -C "${TEST_REPO}" add .
git -C "${TEST_REPO}" commit -m "release fixture" >/dev/null
git -C "${TEST_REPO}" tag v0.0.1

receipt_path="${TMP_DIR}/mesh-receipt.json"
python3 "${ROOT_DIR}/scripts/tests/test_mesh_release.py" --fixture "$(git -C "${TEST_REPO}" rev-parse HEAD)" >"$receipt_path"
export IRIS_STACK_GATE_RECEIPT="$receipt_path"

# A stale or missing mesh receipt must stop before either publication channel.
for bad_receipt in missing stale; do
    if [ "$bad_receipt" = missing ]; then
        candidate_receipt="${TMP_DIR}/missing.json"
    else
        candidate_receipt="${TMP_DIR}/stale.json"
        python3 "${ROOT_DIR}/scripts/tests/test_mesh_release.py" --fixture aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa >"$candidate_receipt"
    fi
    if IRIS_STACK_GATE_RECEIPT="$candidate_receipt" TEST_LOG_DIR="${LOG_DIR}" PATH="${BIN_DIR}:$PATH" \
        "${TEST_REPO}/publish_release.sh" --version v0.0.1 >"${TMP_DIR}/stdout.txt" 2>"${TMP_DIR}/stderr.txt"; then
        echo "Publication accepted a $bad_receipt mesh receipt" >&2
        exit 1
    fi
    [ ! -e "${LOG_DIR}/release_to_htree.log" ]
    [ ! -e "${LOG_DIR}/gh.log" ]
done

if TEST_LOG_DIR="${LOG_DIR}" PATH="${BIN_DIR}:$PATH" "${TEST_REPO}/publish_release.sh" --version v0.0.1 >"${TMP_DIR}/stdout.txt" 2>"${TMP_DIR}/stderr.txt"; then
    :
else
    cat "${TMP_DIR}/stderr.txt" >&2
    exit 1
fi

grep -F -- 'v0.0.1' "${LOG_DIR}/release_to_htree.log" >/dev/null
grep -F 'release gate invoked' "${LOG_DIR}/release-gate.log" >/dev/null

grep -F 'auth status' "${LOG_DIR}/gh.log" >/dev/null
grep -F 'release view v0.0.1 --repo mmalmi/hashtree' "${LOG_DIR}/gh.log" >/dev/null
grep -F 'release create v0.0.1' "${LOG_DIR}/gh.log" >/dev/null
grep -F -- '--title v0.0.1' "${LOG_DIR}/gh.log" >/dev/null
grep -F -- '--notes-file' "${LOG_DIR}/gh.log" >/dev/null
grep -F 'release.json' "${LOG_DIR}/gh.log" >/dev/null
grep -F 'install.sh' "${LOG_DIR}/gh.log" >/dev/null
grep -F 'hashtree-aarch64-apple-darwin.tar.gz' "${LOG_DIR}/gh.log" >/dev/null

for upload_failure in 0 1; do
    : >"${LOG_DIR}/gh.log"
    status=0
    TEST_RELEASE_EXISTS=1 TEST_UPLOAD_FAILURE="$upload_failure" TEST_LOG_DIR="${LOG_DIR}" \
        PATH="${BIN_DIR}:$PATH" "${TEST_REPO}/publish_release.sh" --version v0.0.1 \
        >"${TMP_DIR}/stdout.txt" 2>"${TMP_DIR}/stderr.txt" || status=$?

    notes_line="$(grep -n -F -- '--notes-file' "${LOG_DIR}/gh.log" | cut -d: -f1)"
    upload_line="$(grep -n -F 'release upload v0.0.1' "${LOG_DIR}/gh.log" | cut -d: -f1)"
    [ "$notes_line" -lt "$upload_line" ]
    if [ "$upload_failure" -eq 1 ]; then
        [ "$status" -eq 37 ]
        ! grep -F -- '--draft=false' "${LOG_DIR}/gh.log" >/dev/null
    else
        [ "$status" -eq 0 ]
        publish_line="$(grep -n -F 'release edit v0.0.1 --repo mmalmi/hashtree --draft=false --verify-tag' "${LOG_DIR}/gh.log" | cut -d: -f1)"
        [ "$upload_line" -lt "$publish_line" ]
    fi
done

echo "test_publish_release_wrapper.sh passed"
