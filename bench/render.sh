#!/usr/bin/env bash
set -euo pipefail

# Entry point for the TUI render and command-parse benchmark, with the same interface as
# bench/startup.sh. The measured case list, the budgets, and the pass/fail rule live in
# crates/pi-rs-tui/benches/render.rs; this script only builds and runs it.
#
# Usage: bench/render.sh [--iterations <N>] [--json <path>]

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

ARGS=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --json)
      # `cargo bench` runs the benchmark with the package directory as its working directory,
      # so a relative path would land inside crates/. Resolve it here, where the caller's
      # intent is still relative to where they typed the command.
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
      echo "Usage: bench/render.sh [--iterations <N>] [--json <path>]"
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
exec cargo bench --quiet -p pi-rs-tui --bench render -- ${ARGS[@]+"${ARGS[@]}"}
