#!/usr/bin/env bash
set -euo pipefail

# Entry point for the large-session restore benchmark, with the same interface as
# bench/startup.sh and bench/render.sh. The measured cases, budgets, and pass/fail rules
# live in crates/pi-rs-store/benches/restore.rs; this script builds and runs it.
#
# Usage: bench/large_session.sh [--iterations <N>] [--json <path>]

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

ARGS=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --json)
      if [[ -z "${2:-}" ]]; then
        echo "--json needs a path" >&2
        exit 1
      fi
      TARGET="$2"
      case "$TARGET" in
        /*) ARGS+=("--json" "$TARGET") ;;
        *) ARGS+=("--json" "$REPO_ROOT/$TARGET") ;;
      esac
      shift 2
      ;;
    --iterations | -n)
      if [[ -z "${2:-}" ]]; then
        echo "--iterations needs a number" >&2
        exit 1
      fi
      ARGS+=("--iterations" "$2")
      shift 2
      ;;
    --help | -h)
      echo "Usage: bench/large_session.sh [--iterations <N>] [--json <path>]"
      echo "  Exits non-zero when a case exceeds its budget."
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      exit 1
      ;;
  esac
done

cd "$REPO_ROOT"
exec cargo bench --quiet -p pi-rs-store --bench restore -- ${ARGS[@]+"${ARGS[@]}"}
