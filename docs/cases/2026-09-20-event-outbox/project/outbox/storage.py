from __future__ import annotations

from pathlib import Path
import sqlite3
from typing import Any

from .model import encode_payload, event_from_row


class Store:
    def __init__(self, path: str | Path) -> None:
        self.path = Path(path)

    def initialize(self) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        connection = self._connect()
        try:
            connection.execute(
                """
                CREATE TABLE IF NOT EXISTS events (
                    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                    event_id TEXT NOT NULL UNIQUE,
                    topic TEXT NOT NULL,
                    payload TEXT NOT NULL,
                    status TEXT NOT NULL CHECK (status IN ('pending', 'delivered')),
                    attempts INTEGER NOT NULL DEFAULT 0,
                    last_error TEXT,
                    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
                )
                """
            )
            connection.commit()
        finally:
            connection.close()

    def add(self, event: dict[str, Any]) -> tuple[str, dict[str, Any]]:
        payload = encode_payload(event["payload"])
        connection = self._connect()
        try:
            existing = connection.execute(
                "SELECT * FROM events WHERE event_id = ?", (event["event_id"],)
            ).fetchone()
            if existing is not None:
                old = event_from_row(existing)
                if old["topic"] == event["topic"] and encode_payload(old["payload"]) == payload:
                    return "duplicate", old
                return "conflict", old
            cursor = connection.execute(
                "INSERT INTO events (event_id, topic, payload, status) VALUES (?, ?, ?, 'pending')",
                (event["event_id"], event["topic"], payload),
            )
            connection.commit()
            row = connection.execute(
                "SELECT * FROM events WHERE sequence = ?", (cursor.lastrowid,)
            ).fetchone()
            assert row is not None
            return "created", event_from_row(row)
        finally:
            connection.close()

    def get(self, event_id: str) -> dict[str, Any] | None:
        connection = self._connect()
        try:
            row = connection.execute("SELECT * FROM events WHERE event_id = ?", (event_id,)).fetchone()
            return None if row is None else event_from_row(row)
        finally:
            connection.close()

    def pending(self) -> list[dict[str, Any]]:
        connection = self._connect()
        try:
            rows = connection.execute(
                "SELECT * FROM events WHERE status = 'pending' ORDER BY sequence"
            ).fetchall()
            return [event_from_row(row) for row in rows]
        finally:
            connection.close()

    def record_attempt(self, event_id: str, success: bool, error: str | None = None) -> None:
        connection = self._connect()
        try:
            if success:
                cursor = connection.execute(
                    """
                    UPDATE events
                    SET status = 'delivered', attempts = attempts + 1, last_error = NULL
                    WHERE event_id = ? AND status = 'pending'
                    """,
                    (event_id,),
                )
            else:
                cursor = connection.execute(
                    """
                    UPDATE events
                    SET status = 'pending', attempts = attempts + 1, last_error = ?
                    WHERE event_id = ? AND status = 'pending'
                    """,
                    (error or "delivery failed", event_id),
                )
            if cursor.rowcount != 1:
                raise KeyError(event_id)
            connection.commit()
        finally:
            connection.close()

    def _connect(self) -> sqlite3.Connection:
        connection = sqlite3.connect(self.path, timeout=5)
        connection.row_factory = sqlite3.Row
        return connection
