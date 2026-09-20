"""SQLite persistence and explicit delivery state transitions."""

from __future__ import annotations

import json
from pathlib import Path
import sqlite3
import time

from .model import Delivery, DeliveryConflictError, canonical_payload


class Store:
    """A small SQLite boundary with lease transitions kept in one place."""

    def __init__(self, database: str | Path) -> None:
        self.database = str(database)
        if self.database != ":memory:":
            Path(self.database).parent.mkdir(parents=True, exist_ok=True)
        self._initialize()

    def _connect(self) -> sqlite3.Connection:
        connection = sqlite3.connect(self.database, timeout=5)
        connection.row_factory = sqlite3.Row
        connection.execute("PRAGMA busy_timeout = 5000")
        return connection

    def _initialize(self) -> None:
        connection = self._connect()
        try:
            with connection:
                connection.executescript(
                    """
                    CREATE TABLE IF NOT EXISTS deliveries (
                        sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                        delivery_id TEXT NOT NULL UNIQUE,
                        event_type TEXT NOT NULL,
                        payload_json TEXT NOT NULL,
                        status TEXT NOT NULL CHECK(status IN ('pending', 'leased', 'delivered')),
                        attempts INTEGER NOT NULL DEFAULT 0,
                        last_error TEXT,
                        lease_expires_at INTEGER
                    )
                    """
                )
        finally:
            connection.close()

    @staticmethod
    def _now_ms() -> int:
        return int(time.time() * 1000)

    @staticmethod
    def _delivery(row: sqlite3.Row) -> Delivery:
        payload = json.loads(row["payload_json"])
        if not isinstance(payload, dict):
            raise ValueError("stored payload is not an object")
        return Delivery(
            delivery_id=row["delivery_id"],
            event_type=row["event_type"],
            payload=payload,
            status=row["status"],
            attempts=row["attempts"],
            last_error=row["last_error"],
            lease_expires_at=row["lease_expires_at"],
        )

    @staticmethod
    def _release_expired(connection: sqlite3.Connection, now_ms: int) -> None:
        connection.execute(
            """
            UPDATE deliveries
               SET status = 'pending', lease_expires_at = NULL
             WHERE status = 'leased' AND lease_expires_at <= ?
            """,
            (now_ms,),
        )

    def admit(self, delivery_id: str, event_type: str, payload: dict[str, object]) -> tuple[Delivery, bool]:
        """Insert a delivery or return an identical existing row."""

        payload_json = canonical_payload(payload)
        connection = self._connect()
        try:
            with connection:
                try:
                    connection.execute(
                        """
                        INSERT INTO deliveries
                            (delivery_id, event_type, payload_json, status, attempts)
                        VALUES (?, ?, ?, 'pending', 0)
                        """,
                        (delivery_id, event_type, payload_json),
                    )
                    created = True
                except sqlite3.IntegrityError:
                    row = connection.execute(
                        """
                        SELECT delivery_id, event_type, payload_json, status,
                               attempts, last_error, lease_expires_at
                          FROM deliveries WHERE delivery_id = ?
                        """,
                        (delivery_id,),
                    ).fetchone()
                    if row is None:
                        raise
                    if row["event_type"] != event_type or row["payload_json"] != payload_json:
                        raise DeliveryConflictError("delivery_id already contains different content")
                    created = False
                row = connection.execute(
                    """
                    SELECT delivery_id, event_type, payload_json, status,
                           attempts, last_error, lease_expires_at
                      FROM deliveries WHERE delivery_id = ?
                    """,
                    (delivery_id,),
                ).fetchone()
                if row is None:
                    raise RuntimeError("inserted delivery could not be read")
                return self._delivery(row), created
        finally:
            connection.close()

    def get(self, delivery_id: str, now_ms: int | None = None) -> Delivery | None:
        """Read a delivery, making expired leases available first."""

        connection = self._connect()
        try:
            with connection:
                self._release_expired(connection, self._now_ms() if now_ms is None else now_ms)
                row = connection.execute(
                    """
                    SELECT delivery_id, event_type, payload_json, status,
                           attempts, last_error, lease_expires_at
                      FROM deliveries WHERE delivery_id = ?
                    """,
                    (delivery_id,),
                ).fetchone()
                return None if row is None else self._delivery(row)
        finally:
            connection.close()

    def claim_next(
        self,
        lease_seconds: int,
        now_ms: int | None = None,
        exclude_ids: set[str] | frozenset[str] | None = None,
    ) -> Delivery | None:
        """Atomically claim the oldest available delivery."""

        if not 1 <= lease_seconds <= 300:
            raise ValueError("lease_seconds must be between 1 and 300")
        current = self._now_ms() if now_ms is None else now_ms
        connection = self._connect()
        try:
            with connection:
                self._release_expired(connection, current)
                query = "SELECT sequence FROM deliveries WHERE status = 'pending'"
                parameters: list[str] = []
                if exclude_ids:
                    placeholders = ",".join("?" for _ in exclude_ids)
                    query += f" AND delivery_id NOT IN ({placeholders})"
                    parameters.extend(sorted(exclude_ids))
                query += " ORDER BY sequence LIMIT 1"
                row = connection.execute(query, parameters).fetchone()
                if row is None:
                    return None
                connection.execute(
                    """
                    UPDATE deliveries
                       SET status = 'leased', attempts = attempts + 1,
                           lease_expires_at = ?
                     WHERE sequence = ?
                    """,
                    (current + lease_seconds * 1000, row["sequence"]),
                )
                claimed = connection.execute(
                    """
                    SELECT delivery_id, event_type, payload_json, status,
                           attempts, last_error, lease_expires_at
                      FROM deliveries WHERE sequence = ?
                    """,
                    (row["sequence"],),
                ).fetchone()
                if claimed is None:
                    raise RuntimeError("claimed delivery could not be read")
                return self._delivery(claimed)
        finally:
            connection.close()

    def finish(self, delivery_id: str, success: bool, error: str | None = None) -> Delivery:
        """Finalize a leased delivery as delivered or retryable pending."""

        connection = self._connect()
        try:
            with connection:
                if success:
                    connection.execute(
                        """
                        UPDATE deliveries
                           SET status = 'delivered', last_error = NULL,
                               lease_expires_at = NULL
                         WHERE delivery_id = ? AND status = 'leased'
                        """,
                        (delivery_id,),
                    )
                else:
                    message = (error or "sink delivery failed").strip()[:500]
                    connection.execute(
                        """
                        UPDATE deliveries
                           SET status = 'pending', last_error = ?,
                               lease_expires_at = NULL
                         WHERE delivery_id = ? AND status = 'leased'
                        """,
                        (message or "sink delivery failed", delivery_id),
                    )
                row = connection.execute(
                    """
                    SELECT delivery_id, event_type, payload_json, status,
                           attempts, last_error, lease_expires_at
                      FROM deliveries WHERE delivery_id = ?
                    """,
                    (delivery_id,),
                ).fetchone()
                if row is None:
                    raise KeyError(delivery_id)
                return self._delivery(row)
        finally:
            connection.close()
