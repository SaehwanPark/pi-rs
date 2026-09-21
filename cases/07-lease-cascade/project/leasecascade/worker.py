"""Bounded direct-argv worker for Lease Cascade jobs."""

from __future__ import annotations

import json
import subprocess
import sys
from typing import Any

from .storage import InputResolutionError, Store


def run(db: str, sink: str, sink_args: list[str], lease_seconds: int) -> int:
    store = Store(db)
    attempted: set[tuple[str, str]] = set()
    had_failure = False
    while True:
        job = store.claim_next(attempted, lease_seconds)
        if job is None:
            return 1 if had_failure else 0
        key = (job["pipeline_id"], job["job_id"])
        attempted.add(key)
        if not job.get("claimed", True):
            message = job.get("local_error", "local job resolution failed")
            print(f"{job['job_id']}: {message}", file=sys.stderr)
            had_failure = True
            continue
        try:
            inputs = store.resolve_inputs(job)
        except InputResolutionError as exc:
            store.finish_terminal(job, str(exc))
            print(f"{job['job_id']}: {exc}", file=sys.stderr)
            had_failure = True
            continue

        request = {
            "pipeline_id": job["pipeline_id"],
            "job_id": job["job_id"],
            "kind": job["kind"],
            "payload": job["payload"],
            "depends_on": job["depends_on"],
            "inputs": inputs,
            "fan_in": job["fan_in"],
        }
        outcome, detail = _invoke_sink(sink, sink_args, request, job["job_id"])
        if outcome == "success":
            store.finish_success(job, detail)
        elif outcome == "terminal":
            store.finish_terminal(job, detail)
            print(f"{job['job_id']}: {detail}", file=sys.stderr)
            had_failure = True
        else:
            store.finish_retryable(job, detail)
            print(f"{job['job_id']}: {detail}", file=sys.stderr)
            had_failure = True


def _invoke_sink(
    program: str,
    args: list[str],
    request: dict[str, Any],
    expected_job_id: str,
) -> tuple[str, Any]:
    command = [program, *args]
    line = json.dumps(request, ensure_ascii=False, separators=(",", ":")) + "\n"
    try:
        process = subprocess.Popen(
            command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
        )
        try:
            stdout, _stderr = process.communicate(line, timeout=30)
        except subprocess.TimeoutExpired:
            process.kill()
            process.communicate()
            return "retryable", "sink timed out"
    except OSError as exc:
        return "retryable", f"sink process failed: {exc}"

    if process.returncode != 0:
        return "retryable", f"sink exited with status {process.returncode}"
    lines = [item for item in stdout.splitlines() if item.strip()]
    if len(lines) != 1:
        return "retryable", "sink returned no single JSON response"
    try:
        response = json.loads(lines[0])
    except json.JSONDecodeError:
        return "retryable", "sink returned malformed JSON"
    if not isinstance(response, dict) or response.get("job_id") != expected_job_id:
        return "retryable", "sink response job_id did not match the claimed job"
    if response.get("ok") is False:
        message = response.get("error")
        if not isinstance(message, str) or not message.strip():
            message = "sink rejected the job"
        outcome = "terminal" if response.get("retryable", True) is False else "retryable"
        return outcome, message.strip()
    if response.get("ok") is not True or not isinstance(response.get("output"), dict):
        return "retryable", "successful sink response must contain an object output"
    return "success", response["output"]
