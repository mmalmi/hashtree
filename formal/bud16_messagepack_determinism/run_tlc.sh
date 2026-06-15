#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
JAR_PATH="${ROOT_DIR}/.tools/tla2tools.jar"
MODE="all"

usage() {
  cat <<'USAGE'
Usage:
  ./formal/bud16_messagepack_determinism/run_tlc.sh [--mode all|ci]

Modes:
  all  Run all pass-expected configs.
  ci   Run pass-expected configs; fail on any error.
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --mode)
      MODE="${2:-}"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage
      exit 1
      ;;
  esac
done

if [[ "${MODE}" != "all" && "${MODE}" != "ci" ]]; then
  echo "Invalid mode: ${MODE}" >&2
  usage
  exit 1
fi

mkdir -p "${ROOT_DIR}/.tools"

if [[ ! -f "${JAR_PATH}" ]]; then
  curl -fsSL \
    -o "${JAR_PATH}" \
    "https://github.com/tlaplus/tlaplus/releases/download/v1.8.0/tla2tools.jar"
fi

run_cfg() {
  local cfg="$1"
  echo
  echo "=== Running ${cfg} ==="
  java -cp "${JAR_PATH}" tlc2.TLC \
    -cleanup \
    -deadlock \
    -config "${cfg}" \
    Bud16MessagePackDeterminism.tla
}

cd "${ROOT_DIR}"

run_cfg Bud16MessagePackDeterminism.fixed.cfg
