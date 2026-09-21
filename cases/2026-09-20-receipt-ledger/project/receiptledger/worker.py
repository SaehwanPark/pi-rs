"""Bounded direct-argv worker with stale-claim fencing and receipt validation."""

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
            if not store.finish_terminal(job, "local input resolution failed"):
                print(f"{job['job_id']}: stale lease ignored", file=sys.stderr)
            else:
                print(f"{job['job_id']}: {exc}", file=sys.stderr)
            had_failure = True
            continue

        sink_request = {
            "pipeline_id": job["pipeline_id"],
            "job_id": job["job_id"],
            "kind": job["kind"],
            "payload": job["payload"],
            "depends_on": job["depends_on"],
            "inputs": inputs,
            "fan_in": job["fan_in"],
            "delivery_key": job["delivery_key"],
        }
        outcome, detail = _invoke_sink(
            sink,
            sink_args,
            sink_request,
            job["job_id"],
            job["delivery_key"],
        )
        if outcome == "success":
            accepted = store.finish_success(job, detail["output"], detail["receipt_id"])
        elif outcome == "terminal":
            accepted = store.finish_terminal(job, detail)
        else:
            accepted = store.finish_retryable(
                job,
                detail["message"],
                detail.get("audit_outcome", "observed_failure"),
            )
        if not accepted:
            print(f"{job['job_id']}: stale lease ignored", file=sys.stderr)
            had_failure = True
        elif outcome != "success":
            message = detail if isinstance(detail, str) else detail["message"]
            print(f"{job['job_id']}: {message}", file=sys.stderr)
            had_failure = True


def _retry(message: str, outcome: str = "observed_failure") -> tuple[str, dict[str, str]]:
    return "retryable", {"message": message, "audit_outcome": outcome}


def _invoke_sink(
    program: str,
    args: list[str],
    request: dict[str, Any],
    expected_job_id: str,
    expected_delivery_key: str,
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
            return _retry("sink timed out", "unknown")
    except OSError:
        return _retry("sink process failed", "unknown")

    if process.returncode != 0:
        return _retry("sink ended before an acknowledged response", "unknown")
    lines = [item for item in stdout.splitlines() if item.strip()]
    if len(lines) != 1:
        return _retry("sink returned no single JSON response", "unknown")
    try:
        response = json.loads(lines[0])
    except json.JSONDecodeError:
        return _retry("sink returned malformed JSON", "unknown")
    if not isinstance(response, dict) or response.get("job_id") != expected_job_id:
        return _retry("sink response job_id did not match the claimed job", "unknown")
    if response.get("delivery_key") != expected_delivery_key:
        return _retry("sink response delivery_key did not match the claimed job", "unknown")
    if response.get("ok") is False:
        message = response.get("error")
        if not isinstance(message, str) or not message.strip():
            message = "sink rejected the job"
        if response.get("retryable", True) is False:
            return "terminal", "sink rejected the job"
        return _retry("sink requested a retry", "observed_failure")
    if response.get("ok") is not True or not isinstance(response.get("output"), dict):
        return _retry("successful sink response must contain an object output", "unknown")
    receipt_id = response.get("receipt_id")
    if not isinstance(receipt_id, str) or not receipt_id.strip():
        return _retry("successful sink response must contain a receipt_id", "unknown")
    return "success", {"output": response["output"], "receipt_id": receipt_id.strip()}
