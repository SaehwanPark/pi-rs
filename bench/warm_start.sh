#!/usr/bin/env bash
# Warm-start benchmark for pi-rs: relaunching a process that continues a stored session.
#
# Definition (pinned in docs/SLICE_WARM_START.md)
#   Warm start is the time from exec to exit for
#     pi-rs run --config <file> --cwd <workspace> --resume <id> --prompt <text>
#   against a session that already exists in the store, compared against the same
#   argv without --resume against an empty store. It measures resolve + Store::restore
#   + context rebuild: the launch-path work that continuing a recorded session added.
#
# What this is not
#   It is not a cold-start number. The page cache is warm and the binary inode is warm
#   (bench/cold_start.sh measures the first exec of a fresh inode); the fixture run
#   touches the store, the config, and the shared objects before any sample is taken,
#   and nothing here drops caches. Both arms also pay one provider round trip, which
#   is part of `run` and cannot be separated from it without changing the argv.
#
# No budget
#   There is no pass/fail threshold here and none should be added. bench/cold_start.sh
#   shows why: its own numbers would not hold a sign. The noise line below compares the
#   median delta against the run-to-run spread of the two arms and says plainly, when
#   that is the case, that the delta is inside the noise. That is a description, not a
#   gate.
#
# Where this runs
#   This is a pre-merge gate on a dev machine, not a CI check. If the fixture run that
#   creates the session cannot complete (no provider configured, endpoint unreachable),
#   it prints `SKIP: <one-line reason>` and exits 0. A red CI because someone's laptop
#   has no endpoint is a false alarm.
#
# Safety
#   The config named by $PI_RS_CONFIG is read, never written. Its `state_dir` is
#   rewritten into a throwaway store under a mktemp workdir, so this script never opens
#   the real store and never touches an existing session. The workdir is removed on
#   exit. The workspace passed to --cwd is inside the same workdir.
#
# Usage: bench/warm_start.sh [--json <path>] [--iterations <N>]
#   N is the number of iterations; each iteration contributes one resume sample and one
#   fresh-session sample (default 5).
#
# Config: PI_RS_CONFIG=<file> (default ~/.config/pi-rs/config.json)
#   The same JSON a user passes to `pi-rs run --config`. Only `state_dir` is overridden.
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
      echo "Usage: bench/warm_start.sh [--json <path>] [--iterations <N>]"
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

# Created here rather than in python so the trap below owns it even if python dies.
WORKDIR="$(mktemp -d -t pi-rs-warm-start-XXXXXX)"
trap 'rm -rf "${WORKDIR}"' EXIT INT TERM

BINARY="$BINARY" \
  ITERATIONS="$ITERATIONS" \
  JSON_OUT="$JSON_OUT" \
  CONFIG="${PI_RS_CONFIG:-${HOME}/.config/pi-rs/config.json}" \
  WORKDIR="$WORKDIR" \
  python3 - <<'PY'
import json
import os
import shutil
import statistics
import subprocess
import sys
import time

binary = os.environ["BINARY"]
iterations = int(os.environ["ITERATIONS"])
json_out = os.environ["JSON_OUT"]
config_path = os.environ["CONFIG"]
workdir = os.environ["WORKDIR"]

# One tiny turn with no tool call, so the provider work is the same in both arms and
# the difference stays on the launch path.
PROMPT = "Reply with exactly: ok"
# A sample that does not finish in this long is not a slow measurement, it is a hung
# endpoint. Failing loudly beats averaging a timeout into the numbers.
RUN_TIMEOUT_S = 120

def skip(reason):
    print(f"SKIP: {reason}")
    sys.exit(0)

def reason_line(text):
    """The line that names a failure. `pi-rs run` writes its one-line failure as
    `error: <text>` on stderr, after any transcript chrome, so prefer that."""
    lines = [line.strip() for line in (text or "").splitlines() if line.strip()]
    for line in lines:
        if line.startswith("error:"):
            return line
    return lines[0] if lines else "no output"


def stat_block(samples):
    return {
        "min": min(samples),
        "mean": statistics.mean(samples),
        "median": statistics.median(samples),
        "max": max(samples),
    }


def spread(samples):
    """Run-to-run spread: interquartile range, full range when there are too few
    samples for quartiles. Used only to describe the noise around the delta."""
    if len(samples) < 4:
        return max(samples) - min(samples)
    quartiles = statistics.quantiles(samples, n=4, method="inclusive")
    return quartiles[2] - quartiles[0]


def run_pi(config_file, resume_id=None):
    """One timed exec-to-exit of `pi-rs run`.

    The argv is the one named in the definition, so both arms differ only by
    `--resume <id>`. stdin is detached and stdout is discarded exactly as
    bench/startup.sh discards them; stderr is captured only so a failure can name
    itself. Returns (elapsed_ms, returncode, first stderr line), with elapsed_ms
    None when the run hit the timeout.
    """
    argv = [binary, "run", "--config", config_file, "--cwd", workspace, "--prompt", PROMPT]
    if resume_id is not None:
        argv[4:4] = ["--resume", resume_id]
    t0 = time.perf_counter()
    try:
        res = subprocess.run(
            argv,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            timeout=RUN_TIMEOUT_S,
        )
    except subprocess.TimeoutExpired:
        return None, None, f"timed out after {RUN_TIMEOUT_S}s"
    return (
        (time.perf_counter() - t0) * 1000.0,
        res.returncode,
        reason_line(res.stderr.decode("utf-8", "replace")),
    )


def failure_text(elapsed_ms, returncode, stderr_line):
    """One line naming why a run is not a usable sample."""
    if elapsed_ms is None:
        return stderr_line
    return f"exited {returncode}: {stderr_line}"


if not os.path.isfile(binary) or not os.access(binary, os.X_OK):
    sys.stderr.write(f"Binary not executable: {binary}\n")
    sys.exit(1)

if iterations < 1:
    sys.stderr.write(f"iterations must be >= 1, got {iterations}\n")
    sys.exit(1)

if not os.path.isfile(config_path):
    skip(f"no provider config at {config_path} (set PI_RS_CONFIG)")

try:
    with open(config_path) as f:
        base_config = json.load(f)
except (OSError, ValueError) as error:
    skip(f"cannot read config {config_path}: {error}")

# The fixture store, and the two per-arm stores rebuilt from it before each sample.
fixture_store = os.path.join(workdir, "fixture-store")
resume_store = os.path.join(workdir, "resume-store")
fresh_store = os.path.join(workdir, "fresh-store")
workspace = os.path.join(workdir, "workspace")
os.makedirs(workspace)


def write_config(name, state_dir):
    """The user's config with `state_dir` pointed at a throwaway store, nothing else."""
    base_config["state_dir"] = state_dir
    path = os.path.join(workdir, name)
    with open(path, "w") as f:
        json.dump(base_config, f, indent=2)
    return path


config_fixture = write_config("config-fixture.json", fixture_store)
config_resume = write_config("config-resume.json", resume_store)
config_fresh = write_config("config-fresh.json", fresh_store)

# Fixture: one real turn creating the session a warm start continues. This is the run
# that decides whether the machine can do the measurement at all.
fixture_ms, fixture_rc, fixture_stderr = run_pi(config_fixture)
if fixture_ms is None or fixture_rc != 0:
    skip(f"fixture run cannot complete: {failure_text(fixture_ms, fixture_rc, fixture_stderr)}")

sessions = os.path.join(fixture_store, "sessions")
try:
    # A session is `<id>.jsonl`; the trace journal `<id>.trace.jsonl` is not a session.
    ids = [n[: -len(".jsonl")] for n in os.listdir(sessions) if n.endswith(".jsonl")]
    ids = [n for n in ids if not n.endswith(".trace")]
except OSError as error:
    skip(f"fixture run wrote no store: {error}")
if len(ids) != 1:
    skip(f"fixture run expected one session in {sessions}, found {len(ids)}")
session_id = ids[0]

resume_times = []
fresh_times = []
for i in range(iterations):
    # Each sample gets the store the definition names: the resume arm restores a session
    # with exactly the one fixture turn, never a log that grows every iteration; the
    # fresh arm starts against an empty store, never the store the last sample wrote.
    shutil.rmtree(resume_store, ignore_errors=True)
    shutil.copytree(fixture_store, resume_store)
    resume_ms, resume_rc, resume_stderr = run_pi(config_resume, resume_id=session_id)
    if resume_ms is None or resume_rc != 0:
        reason = failure_text(resume_ms, resume_rc, resume_stderr)
        sys.stderr.write(f"resume sample #{i + 1} failed: {reason}\n")
        sys.exit(1)
    resume_times.append(resume_ms)

    shutil.rmtree(fresh_store, ignore_errors=True)
    os.makedirs(fresh_store)
    fresh_ms, fresh_rc, fresh_stderr = run_pi(config_fresh)
    if fresh_ms is None or fresh_rc != 0:
        reason = failure_text(fresh_ms, fresh_rc, fresh_stderr)
        sys.stderr.write(f"fresh-session sample #{i + 1} failed: {reason}\n")
        sys.exit(1)
    fresh_times.append(fresh_ms)

resume = stat_block(resume_times)
fresh = stat_block(fresh_times)
delta_median_ms = resume["median"] - fresh["median"]
noise_floor_ms = max(spread(resume_times), spread(fresh_times))

print("Warm-start benchmark (pi-rs):")
print(f"  Resume an existing session ({iterations} runs, --resume {session_id[:8]}...):")
print(f"    min:          {resume['min']:.2f} ms")
print(f"    mean:         {resume['mean']:.2f} ms")
print(f"    median:       {resume['median']:.2f} ms")
print(f"    max:          {resume['max']:.2f} ms")
print(f"  Fresh session, empty store ({iterations} runs, no --resume):")
print(f"    min:          {fresh['min']:.2f} ms")
print(f"    mean:         {fresh['mean']:.2f} ms")
print(f"    median:       {fresh['median']:.2f} ms")
print(f"    max:          {fresh['max']:.2f} ms")
print(f"  Delta (resume median - fresh median): {delta_median_ms:+.2f} ms")
print(f"  Noise floor (larger arm spread): {noise_floor_ms:.2f} ms")
if noise_floor_ms == 0.0:
    print("  Inside noise: cannot tell, the samples have no spread.")
elif abs(delta_median_ms) <= noise_floor_ms:
    print("  Inside noise: yes. The delta is not distinguishable from run-to-run spread.")
else:
    print("  Inside noise: no. The delta is larger than the run-to-run spread.")
print("  Note: not a cold-start number (page cache and binary inode are warm), and")
print("  both arms pay one provider round trip. No budget is applied here.")
print("  See docs/SLICE_WARM_START.md.")

results = {
    "resume_min_ms": round(resume["min"], 3),
    "resume_mean_ms": round(resume["mean"], 3),
    "resume_median_ms": round(resume["median"], 3),
    "resume_max_ms": round(resume["max"], 3),
    "fresh_min_ms": round(fresh["min"], 3),
    "fresh_mean_ms": round(fresh["mean"], 3),
    "fresh_median_ms": round(fresh["median"], 3),
    "fresh_max_ms": round(fresh["max"], 3),
    "delta_median_ms": round(delta_median_ms, 3),
    "noise_floor_ms": round(noise_floor_ms, 3),
    "inside_noise": bool(noise_floor_ms > 0.0 and abs(delta_median_ms) <= noise_floor_ms),
    "resume_samples_ms": [round(t, 3) for t in resume_times],
    "fresh_samples_ms": [round(t, 3) for t in fresh_times],
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
