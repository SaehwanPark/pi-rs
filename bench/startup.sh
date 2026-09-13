#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

JSON_OUT=""
ITERATIONS=10
COLD_MODE=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --cold)
      COLD_MODE=1
      # Default iterations to 5 in cold mode unless overridden
      if [[ "$ITERATIONS" -eq 10 ]]; then
        ITERATIONS=5
      fi
      shift
      ;;
    --json)
      JSON_OUT="$2"
      shift 2
      ;;
    --iterations|-n)
      ITERATIONS="$2"
      shift 2
      ;;
    --help|-h)
      echo "Usage: bench/startup.sh [--cold] [--json <path>] [--iterations <N>]"
      echo "  --cold: benchmark first-exec of fresh binary inodes (multi-iteration)"
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

COLD_MODE="$COLD_MODE" BINARY="$BINARY" ITERATIONS="$ITERATIONS" JSON_OUT="$JSON_OUT" python3 - <<'EOF'
import json
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

binary = os.environ["BINARY"]
iterations = int(os.environ["ITERATIONS"])
json_out = os.environ["JSON_OUT"]
cold_mode = int(os.environ.get("COLD_MODE", "0"))

if not os.path.isfile(binary) or not os.access(binary, os.X_OK):
  sys.stderr.write(f"Binary not executable: {binary}\n")
  sys.exit(1)

def exec_once(path):
  t0 = time.perf_counter()
  res = subprocess.run([path], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
  return (time.perf_counter() - t0) * 1000.0, res.returncode

def stat_block(samples):
  return {
    "min": min(samples),
    "mean": statistics.mean(samples),
    "median": statistics.median(samples),
    "max": max(samples),
  }

if cold_mode == 1:
  cold_times = []
  warm_times = []
  workdir = tempfile.mkdtemp(prefix="pi-rs-cold-start-")
  try:
    for i in range(iterations):
      probe = os.path.join(workdir, f"pi-rs-cold-{i}")
      shutil.copyfile(binary, probe)
      os.chmod(probe, 0o755)

      cold_ms, rc = exec_once(probe)
      if rc != 0:
        sys.stderr.write(f"cold exec #{i + 1} exited with code {rc}\n")
        sys.exit(rc)
      cold_times.append(cold_ms)

      for n in (2, 3):
        warm_ms, rc = exec_once(probe)
        if rc != 0:
          sys.stderr.write(f"warm exec #{n} of iteration {i + 1} exited with code {rc}\n")
          sys.exit(rc)
        warm_times.append(warm_ms)
  finally:
    shutil.rmtree(workdir, ignore_errors=True)

  cold = stat_block(cold_times)
  warm = stat_block(warm_times)
  delta_median_ms = cold["median"] - warm["median"]

  print("Cold-start benchmark (pi-rs):")
  print(f"  Cold startup ({iterations} fresh inodes, first exec each):")
  print(f"    min:          {cold['min']:.2f} ms")
  print(f"    mean:         {cold['mean']:.2f} ms")
  print(f"    median:       {cold['median']:.2f} ms")
  print(f"    max:          {cold['max']:.2f} ms")
  print(f"  Warm startup ({len(warm_times)} runs, execs #2-#3 of same inode):")
  print(f"    min:          {warm['min']:.2f} ms")
  print(f"    mean:         {warm['mean']:.2f} ms")
  print(f"    median:       {warm['median']:.2f} ms")
  print(f"    max:          {warm['max']:.2f} ms")
  print(f"  Delta (cold median - warm median): {delta_median_ms:+.2f} ms")
  print("  Note: first-exec-of-a-new-inode tax only; the page cache is never")
  print("  dropped (needs root). See header and docs/SLICE_COLD_START.md.")

  results = {
    "cold_mode": True,
    "cold_min_ms": round(cold["min"], 3),
    "cold_mean_ms": round(cold["mean"], 3),
    "cold_median_ms": round(cold["median"], 3),
    "cold_max_ms": round(cold["max"], 3),
    "warm_min_ms": round(warm["min"], 3),
    "warm_mean_ms": round(warm["mean"], 3),
    "warm_median_ms": round(warm["median"], 3),
    "warm_max_ms": round(warm["max"], 3),
    "delta_median_ms": round(delta_median_ms, 3),
    "cold_samples_ms": [round(t, 3) for t in cold_times],
    "iterations": iterations,
  }
else:
  # Cold run (first execution of built binary)
  cold_ms, rc = exec_once(binary)
  if rc != 0:
    sys.stderr.write(f"Binary exited with code {rc}\n")
    sys.exit(rc)

  # Warm runs
  warm_times = []
  for _ in range(iterations):
    warm_ms, rc = exec_once(binary)
    if rc != 0:
      sys.stderr.write(f"Binary exited with code {rc}\n")
      sys.exit(rc)
    warm_times.append(warm_ms)

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
