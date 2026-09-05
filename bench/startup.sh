#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

JSON_OUT=""
ITERATIONS=10

while [[ $# -gt 0 ]]; do
  case "$1" in
    --json)
      JSON_OUT="$2"
      shift 2
      ;;
    --iterations|-n)
      ITERATIONS="$2"
      shift 2
      ;;
    --help|-h)
      echo "Usage: bench/startup.sh [--json <path>] [--iterations <N>]"
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      exit 1
      ;;
  esac
done

cd "$REPO_ROOT"

cargo build --release --bin pi-rs --quiet

BINARY="${REPO_ROOT}/target/release/pi-rs"

python3 - <<EOF
import json
import os
import statistics
import subprocess
import sys
import time

binary = "${BINARY}"
iterations = ${ITERATIONS}
json_out = "${JSON_OUT}"

if not os.path.isfile(binary) or not os.access(binary, os.X_OK):
  sys.stderr.write(f"Binary not executable: {binary}\n")
  sys.exit(1)

# Cold run (first execution)
t0 = time.perf_counter()
res = subprocess.run([binary], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
cold_ms = (time.perf_counter() - t0) * 1000.0

if res.returncode != 0:
  sys.stderr.write(f"Binary exited with code {res.returncode}\n")
  sys.exit(res.returncode)

# Warm runs
warm_times = []
for _ in range(iterations):
  t0 = time.perf_counter()
  subprocess.run([binary], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
  warm_times.append((time.perf_counter() - t0) * 1000.0)

warm_mean_ms = statistics.mean(warm_times)
warm_median_ms = statistics.median(warm_times)
warm_min_ms = min(warm_times)
warm_max_ms = max(warm_times)

print("Startup benchmark (pi-rs):")
print(f"  Cold startup:   {cold_ms:.2f} ms")
print(f"  Warm startup ({iterations} runs):")
print(f"    min:          {warm_min_ms:.2f} ms")
print(f"    mean:         {warm_mean_ms:.2f} ms")
print(f"    median:       {warm_median_ms:.2f} ms")
print(f"    max:          {warm_max_ms:.2f} ms")

results = {
  "cold_ms": round(cold_ms, 3),
  "warm_mean_ms": round(warm_mean_ms, 3),
  "warm_median_ms": round(warm_median_ms, 3),
  "warm_min_ms": round(warm_min_ms, 3),
  "warm_max_ms": round(warm_max_ms, 3),
  "iterations": iterations,
}

if json_out:
  out_dir = os.path.dirname(json_out)
  if out_dir:
    os.makedirs(out_dir, exist_ok=True)
  with open(json_out, "w") as f:
    json.dump(results, f, indent=2)
  print(f"Results written to {json_out}")
EOF
