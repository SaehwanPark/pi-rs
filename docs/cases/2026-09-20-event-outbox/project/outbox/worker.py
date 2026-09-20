from __future__ import annotations

import json
from pathlib import Path
import subprocess
from typing import Any, Sequence

from .storage import Store


def run_worker(db: str | Path, sink: str, sink_args: Sequence[str]) -> int:
    store = Store(db)
    store.initialize()
    pending = store.pending()
    if not pending:
        return 0

    try:
        process = subprocess.Popen(
            [sink, *sink_args],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            encoding="utf-8",
            bufsize=1,
        )
    except OSError as exc:
        for event in pending:
            store.record_attempt(event["event_id"], False, f"could not start sink: {exc}")
        return 1

    failed = False
    try:
        assert process.stdin is not None
        assert process.stdout is not None
        for event in pending:
            ok, error = _deliver_one(process, event)
            store.record_attempt(event["event_id"], ok, error)
            failed = failed or not ok
        process.stdin.close()
        try:
            return_code = process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
            return_code = 1
        if return_code != 0:
            failed = True
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
        if process.stdout is not None:
            process.stdout.close()
        if process.stderr is not None:
            process.stderr.close()
    return 1 if failed else 0


def _deliver_one(process: subprocess.Popen[str], event: dict[str, Any]) -> tuple[bool, str | None]:
    assert process.stdin is not None
    assert process.stdout is not None
    request = {key: event[key] for key in ("event_id", "topic", "payload")}
    try:
        process.stdin.write(json.dumps(request, ensure_ascii=False, sort_keys=True) + "\n")
        process.stdin.flush()
        line = process.stdout.readline()
        if not line:
            return False, "sink closed stdout before replying"
        response = json.loads(line)
        if not isinstance(response, dict):
            return False, "sink response must be a JSON object"
        if response.get("event_id") != event["event_id"]:
            return False, "sink response event_id did not match request"
        if response.get("ok") is True:
            return True, None
        return False, str(response.get("error") or "sink rejected event")
    except (BrokenPipeError, OSError) as exc:
        return False, f"sink I/O failed: {exc}"
    except (json.JSONDecodeError, UnicodeDecodeError) as exc:
        return False, f"invalid sink response: {exc}"
