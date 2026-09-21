"""Tamper-evident, read-only operational audit projections."""

from __future__ import annotations

import hashlib
import json
import sqlite3
from pathlib import Path
from typing import Any


ZERO_HASH = "0" * 64
AUDIT_KINDS = {
    "pipeline_admitted",
    "job_claimed",
    "lease_reclaimed",
    "job_succeeded",
    "job_retryable_failure",
    "job_failed",
    "job_blocked",
    "stale_finalization_rejected",
}


class AuditError(ValueError):
    """The durable audit ledger is missing or fails integrity verification."""


def canonical_event(event: dict[str, Any]) -> str:
    return json.dumps(event, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def append_event(
    connection: sqlite3.Connection,
    kind: str,
    *,
    pipeline_id: str | None = None,
    job_id: str | None = None,
    delivery_key: str | None = None,
    status: str | None = None,
    attempts: int | None = None,
    outcome: str | None = None,
    detail: str | None = None,
) -> dict[str, Any]:
    """Append one safe event; callers must already own a write transaction."""

    if kind not in AUDIT_KINDS:
        raise AuditError(f"unsupported audit event kind: {kind}")
    row = connection.execute(
        "SELECT seq, event_hash FROM audit_events ORDER BY seq DESC LIMIT 1"
    ).fetchone()
    seq = int(row[0]) + 1 if row is not None else 1
    previous = str(row[1]) if row is not None else ZERO_HASH
    event: dict[str, Any] = {"kind": kind, "seq": seq}
    values = {
        "pipeline_id": pipeline_id,
        "job_id": job_id,
        "delivery_key": delivery_key,
        "status": status,
        "attempts": attempts,
        "outcome": outcome,
        "detail": detail,
    }
    for key, value in values.items():
        if value is not None:
            event[key] = value
    event_json = canonical_event(event)
    event_hash = hashlib.sha256((previous + "\n" + event_json).encode("utf-8")).hexdigest()
    connection.execute(
        "INSERT INTO audit_events (seq, event_json, prev_hash, event_hash) VALUES (?, ?, ?, ?)",
        (seq, event_json, previous, event_hash),
    )
    return event


def _read_only_connection(path: str | Path) -> sqlite3.Connection:
    database = Path(path).resolve().as_uri() + "?mode=ro"
    connection = sqlite3.connect(database, uri=True)
    connection.row_factory = sqlite3.Row
    return connection


def verify_database(path: str | Path) -> tuple[int, str | None]:
    """Verify every row without opening the database for writes."""

    try:
        connection = _read_only_connection(path)
    except sqlite3.Error as exc:
        raise AuditError(f"audit database cannot be opened read-only: {exc}") from exc
    try:
        try:
            rows = connection.execute(
                "SELECT seq, event_json, prev_hash, event_hash "
                "FROM audit_events ORDER BY seq"
            ).fetchall()
        except sqlite3.Error as exc:
            raise AuditError(f"audit table cannot be read: {exc}") from exc
        previous = ZERO_HASH
        for expected_seq, row in enumerate(rows, 1):
            seq = int(row["seq"])
            if seq != expected_seq:
                raise AuditError(f"audit sequence mismatch at row {expected_seq}: found {seq}")
            if row["prev_hash"] != previous:
                raise AuditError(f"audit previous hash mismatch at sequence {seq}")
            try:
                event = json.loads(row["event_json"])
            except (TypeError, json.JSONDecodeError) as exc:
                raise AuditError(f"audit event {seq} is not valid JSON") from exc
            if not isinstance(event, dict) or event.get("seq") != seq:
                raise AuditError(f"audit event {seq} has invalid sequence metadata")
            if event.get("kind") not in AUDIT_KINDS:
                raise AuditError(f"audit event {seq} has unsupported kind")
            if row["event_json"] != canonical_event(event):
                raise AuditError(f"audit event {seq} is not canonical JSON")
            expected_hash = hashlib.sha256(
                (row["prev_hash"] + "\n" + row["event_json"]).encode("utf-8")
            ).hexdigest()
            if row["event_hash"] != expected_hash:
                raise AuditError(f"audit hash mismatch at sequence {seq}")
            previous = row["event_hash"]
        return len(rows), previous if rows else None
    finally:
        connection.close()


def tail_database(path: str | Path, count: int) -> list[str]:
    if count < 0:
        raise AuditError("audit tail count must not be negative")
    try:
        connection = _read_only_connection(path)
    except sqlite3.Error as exc:
        raise AuditError(f"audit database cannot be opened read-only: {exc}") from exc
    try:
        rows = connection.execute(
            "SELECT event_json FROM audit_events ORDER BY seq DESC LIMIT ?", (count,)
        ).fetchall()
        return [str(row["event_json"]) for row in reversed(rows)]
    except sqlite3.Error as exc:
        raise AuditError(f"audit table cannot be read: {exc}") from exc
    finally:
        connection.close()
