from __future__ import annotations

import json
import subprocess
from typing import Any

from .storage import Store


def _error_text(value: Any, fallback: str) -> str:
    if isinstance(value, str) and value.strip():
        return value.strip()[:240]
    return fallback


def invoke_sink(command: list[str], job: dict[str, Any]) -> tuple[bool, bool, str | None]:
    request_line = json.dumps(
        {
            "batch_id": job["batch_id"],
            "job_id": job["job_id"],
            "kind": job["kind"],
            "payload": job["payload"],
            "depends_on": job["depends_on"],
        },
        sort_keys=True,
        separators=(",", ":"),
    ) + "\n"
    try:
        process = subprocess.Popen(
            command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
        )
        stdout, stderr = process.communicate(request_line, timeout=30)
    except (OSError, subprocess.TimeoutExpired) as exc:
        if "process" in locals() and process.poll() is None:
            process.kill()
            process.communicate()
        return False, True, _error_text(str(exc), "sink process failed")
    if process.returncode != 0:
        return False, True, _error_text(stderr, f"sink exited with status {process.returncode}")
    lines = [line for line in stdout.splitlines() if line.strip()]
    if len(lines) != 1:
        return False, True, "sink did not return exactly one JSON response"
    try:
        response = json.loads(lines[0])
    except json.JSONDecodeError:
        return False, True, "sink returned malformed JSON"
    if not isinstance(response, dict) or response.get("job_id") != job["job_id"]:
        return False, True, "sink response job_id did not match"
    if response.get("ok") is True:
        return True, False, None
    retryable = response.get("retryable", True)
    if not isinstance(retryable, bool):
        retryable = True
    return False, retryable, _error_text(response.get("error"), "sink rejected job")


def run_worker(db: str, sink: str, sink_args: list[str], lease_seconds: int) -> int:
    if not 1 <= lease_seconds <= 300:
        raise ValueError("lease-seconds must be between 1 and 300")
    store = Store(db)
    failed = False
    attempted: set[tuple[str, str]] = set()
    command = [sink, *sink_args]
    try:
        while True:
            job = store.claim_next(lease_seconds, attempted)
            if job is None:
                break
            batch_id = job["batch_id"]
            attempted.add((batch_id, job["job_id"]))
            success, retryable, error = invoke_sink(command, job)
            store.finalize(
                batch_id,
                job["job_id"],
                success=success,
                retryable=retryable,
                error=error,
            )
            failed = failed or not success
    finally:
        store.close()
    return 1 if failed else 0
