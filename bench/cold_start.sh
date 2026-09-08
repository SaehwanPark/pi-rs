#!/usr/bin/env bash
# Cold-start benchmark for pi-rs: first exec of a fresh binary inode.
#
# Definition (pinned in docs/SLICE_COLD_START.md)
#   Dropping the page cache needs root, which this project does not assume. So
#   cold is defined as: the first execution of a freshly-linked binary inode,
#   with no prior exec of that inode. Each iteration copies
#   target/release/pi-rs to a unique path and times exec #1 of that path
#   against execs #2 and #3 of the same path. That isolates first-exec effects
#   (text page-in, relocation, dynamic linking) from steady state, which is the
#   thing a user on a cold machine actually pays more of.
#
# What this is not
#   It is not a page-cache-cold measurement. The copy destination is written by
#   the same machine that then execs it, so its text pages are already resident
#   when exec #1 runs, and after the first iteration the shared objects
#   (dynamic linker, libc, libstd) stay resident for every later iteration.
#   A real cold-start number would need one of: root `echo 3 > /proc/sys/vm/
#   drop_caches` (or `echo 1`) before each exec, `posix_fadvise(POSIX_FADV_
#   DONTNEED)` / cgroup reclaim control over the binary's pages, or a fresh-VM
#   (or fresh-container) boot harness per iteration. Until then read the delta
#   as "first-exec-of-a-new-inode tax", not "cost of loading from disk".
#
# Where this runs
#   This is a pre-merge gate on a dev machine, not a CI check. CI runners have
#   page-cache behaviour (ephemeral inodes, shared/prewarmed caches, noisy
#   neighbours, and cache state that is not controllable without root) that
#   makes this number meaningless there.
#
# Usage: bench/cold_start.sh [--json <path>] [--iterations <N>]
#   N is the number of fresh-inode iterations (default 5). Each iteration
#   contributes one cold sample and two warm samples.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

JSON_OUT=""
ITERATIONS=5

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
      echo "Usage: bench/cold_start.sh [--json <path>] [--iterations <N>]"
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      exit 1
      ;;
  esac
done

cd "$REPO_ROOT"

# Build once. Same target as bench/startup.sh; keep the tail short.
cargo build --release --bin pi-rs 2>&1 | tail -3

BINARY="${REPO_ROOT}/target/release/pi-rs"

BINARY="$BINARY" ITERATIONS="$ITERATIONS" JSON_OUT="$JSON_OUT" python3 - <<'PY'
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

if not os.path.isfile(binary) or not os.access(binary, os.X_OK):
    sys.stderr.write(f"Binary not executable: {binary}\n")
    sys.exit(1)

if iterations < 1:
    sys.stderr.write(f"iterations must be >= 1, got {iterations}\n")
    sys.exit(1)


def exec_once(path):
    """Trivial invocation, same argv as bench/startup.sh (bare binary, no args).

    stdin is detached so the measurement does not depend on whether the caller
    has a terminal; stdout/stderr are discarded exactly as startup.sh discards
    them.
    """
    t0 = time.perf_counter()
    res = subprocess.run(
        [path],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    return (time.perf_counter() - t0) * 1000.0, res.returncode


def stat_block(samples):
    return {
        "min": min(samples),
        "mean": statistics.mean(samples),
        "median": statistics.median(samples),
        "max": max(samples),
    }


cold_times = []
warm_times = []
workdir = tempfile.mkdtemp(prefix="pi-rs-cold-start-")
try:
    for i in range(iterations):
        probe = os.path.join(workdir, f"pi-rs-cold-{i}")
        # copyfile (not copy2/hardlink) so the probe is a brand-new inode whose
        # first exec is the thing under test.
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

if json_out:
    out_dir = os.path.dirname(json_out)
    if out_dir:
        os.makedirs(out_dir, exist_ok=True)
    with open(json_out, "w") as f:
        json.dump(results, f, indent=2)
    print(f"Results written to {json_out}")
PY
