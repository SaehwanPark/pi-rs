"""Direct-argv sink delivery and bounded lease processing."""

from __future__ import annotations

import json
import subprocess

from .model import Delivery
from .storage import Store


class SinkError(RuntimeError):
    """Raised when a sink does not complete the one-line response protocol."""


def _sink_response(delivery: Delivery, program: str, arguments: list[str]) -> None:
    request = {
        "delivery_id": delivery.delivery_id,
        "event_type": delivery.event_type,
        "payload": delivery.payload,
    }
    try:
        result = subprocess.run(
            [program, *arguments],
            input=json.dumps(request, sort_keys=True, separators=(",", ":")) + "\n",
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError as exc:
        raise SinkError(f"could not start sink: {exc}") from exc
    if result.returncode != 0:
        raise SinkError(f"sink exited with status {result.returncode}")
    lines = result.stdout.splitlines()
    if len(lines) != 1:
        raise SinkError("sink must return exactly one JSON line")
    try:
        response = json.loads(lines[0])
    except json.JSONDecodeError as exc:
        raise SinkError("sink returned malformed JSON") from exc
    if (
        type(response) is not dict
        or response.get("delivery_id") != delivery.delivery_id
        or response.get("ok") is not True
    ):
        raise SinkError("sink returned an unsuccessful or mismatched response")


def run_worker(database: str, program: str, arguments: list[str], lease_seconds: int) -> int:
    """Drain currently available deliveries and return a process exit code."""

    store = Store(database)
    had_failure = False
    attempted: set[str] = set()
    while True:
        delivery = store.claim_next(lease_seconds, exclude_ids=attempted)
        if delivery is None:
            return 1 if had_failure else 0
        attempted.add(delivery.delivery_id)
        try:
            _sink_response(delivery, program, arguments)
        except SinkError as exc:
            store.finish(delivery.delivery_id, success=False, error=str(exc))
            had_failure = True
        else:
            store.finish(delivery.delivery_id, success=True)
