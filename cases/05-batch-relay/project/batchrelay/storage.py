from __future__ import annotations

import json
import sqlite3
import threading
import time
from pathlib import Path
from typing import Any


class BatchConflictError(ValueError):
    """A batch id was reused with different content."""


def canonical_json(value: Any) -> str:
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


class Store:
    def __init__(self, path: str | Path):
        self.path = Path(path)
        if str(self.path) != ":memory:":
            self.path.parent.mkdir(parents=True, exist_ok=True)
        self.connection = sqlite3.connect(self.path if str(self.path) != ":memory:" else ":memory:", check_same_thread=False)
        self.connection.row_factory = sqlite3.Row
        self.connection.execute("PRAGMA foreign_keys = ON")
        self.lock = threading.RLock()
        self._create_schema()

    def _create_schema(self) -> None:
        with self.lock, self.connection:
            self.connection.executescript(
                """
                CREATE TABLE IF NOT EXISTS batches (
                    batch_id TEXT PRIMARY KEY,
                    content_json TEXT NOT NULL,
                    created_at INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS jobs (
                    batch_id TEXT NOT NULL REFERENCES batches(batch_id) ON DELETE CASCADE,
                    job_id TEXT NOT NULL,
                    ordinal INTEGER NOT NULL,
                    kind TEXT NOT NULL,
                    payload_json TEXT NOT NULL,
                    depends_json TEXT NOT NULL,
                    status TEXT NOT NULL,
                    attempts INTEGER NOT NULL DEFAULT 0,
                    last_error TEXT,
                    lease_expires_at INTEGER,
                    PRIMARY KEY(batch_id, job_id)
                );
                CREATE INDEX IF NOT EXISTS jobs_status_idx ON jobs(status);
                """
            )

    def close(self) -> None:
        with self.lock:
            self.connection.close()

    @staticmethod
    def _now_ms() -> int:
        return int(time.time() * 1000)

    def submit(self, batch: dict[str, Any]) -> tuple[dict[str, Any], bool]:
        content = canonical_json(batch)
        with self.lock:
            existing = self.connection.execute(
                "SELECT content_json FROM batches WHERE batch_id = ?", (batch["batch_id"],)
            ).fetchone()
            if existing is not None:
                if existing["content_json"] != content:
                    raise BatchConflictError("batch_id already exists with different content")
                return self.get(batch["batch_id"]), False
            now = self._now_ms()
            try:
                with self.connection:
                    self.connection.execute(
                        "INSERT INTO batches(batch_id, content_json, created_at) VALUES (?, ?, ?)",
                        (batch["batch_id"], content, now),
                    )
                    for ordinal, job in enumerate(batch["jobs"]):
                        self.connection.execute(
                            """
                            INSERT INTO jobs(
                                batch_id, job_id, ordinal, kind, payload_json, depends_json,
                                status, attempts, last_error, lease_expires_at
                            ) VALUES (?, ?, ?, ?, ?, ?, 'pending', 0, NULL, NULL)
                            """,
                            (
                                batch["batch_id"],
                                job["job_id"],
                                ordinal,
                                job["kind"],
                                canonical_json(job["payload"]),
                                canonical_json(job["depends_on"]),
                            ),
                        )
            except sqlite3.IntegrityError as exc:
                raise BatchConflictError("batch_id already exists") from exc
            return self.get(batch["batch_id"]), True

    def _reap_expired_locked(self, now: int) -> None:
        self.connection.execute(
            """
            UPDATE jobs
            SET status = 'pending', lease_expires_at = NULL,
                last_error = COALESCE(last_error, 'lease expired; reclaimed')
            WHERE status = 'leased' AND lease_expires_at <= ?
            """,
            (now,),
        )

    def _block_dependents_locked(self) -> None:
        changed = True
        while changed:
            changed = False
            rows = self.connection.execute(
                "SELECT batch_id, job_id, depends_json FROM jobs WHERE status = 'pending' ORDER BY rowid"
            ).fetchall()
            for row in rows:
                dependencies = json.loads(row["depends_json"])
                if not dependencies:
                    continue
                placeholders = ",".join("?" for _ in dependencies)
                failed = self.connection.execute(
                    f"""
                    SELECT 1 FROM jobs
                    WHERE batch_id = ? AND job_id IN ({placeholders})
                      AND status IN ('failed', 'blocked')
                    LIMIT 1
                    """,
                    (row["batch_id"], *dependencies),
                ).fetchone()
                if failed is not None:
                    self.connection.execute(
                        """
                        UPDATE jobs
                        SET status = 'blocked', lease_expires_at = NULL,
                            last_error = 'blocked by failed dependency'
                        WHERE batch_id = ? AND job_id = ? AND status = 'pending'
                        """,
                        (row["batch_id"], row["job_id"]),
                    )
                    changed = True

    def claim_next(
        self, lease_seconds: int, excluded: set[tuple[str, str]] | None = None
    ) -> dict[str, Any] | None:
        excluded = excluded or set()
        now = self._now_ms()
        with self.lock:
            try:
                self.connection.execute("BEGIN IMMEDIATE")
                self._reap_expired_locked(now)
                self._block_dependents_locked()
                rows = self.connection.execute(
                    "SELECT rowid, * FROM jobs WHERE status = 'pending' ORDER BY rowid"
                ).fetchall()
                selected = None
                for row in rows:
                    if (row["batch_id"], row["job_id"]) in excluded:
                        continue
                    dependencies = json.loads(row["depends_json"])
                    if not dependencies:
                        selected = row
                        break
                    placeholders = ",".join("?" for _ in dependencies)
                    incomplete = self.connection.execute(
                        f"""
                        SELECT 1 FROM jobs
                        WHERE batch_id = ? AND job_id IN ({placeholders})
                          AND status != 'succeeded'
                        LIMIT 1
                        """,
                        (row["batch_id"], *dependencies),
                    ).fetchone()
                    if incomplete is None:
                        selected = row
                        break
                if selected is None:
                    self.connection.commit()
                    return None
                expires = now + lease_seconds * 1000
                self.connection.execute(
                    """
                    UPDATE jobs
                    SET status = 'leased', attempts = attempts + 1, lease_expires_at = ?
                    WHERE batch_id = ? AND job_id = ? AND status = 'pending'
                    """,
                    (expires, selected["batch_id"], selected["job_id"]),
                )
                self.connection.commit()
                job = self._job_from_row(
                    self.connection.execute(
                        "SELECT * FROM jobs WHERE batch_id = ? AND job_id = ?",
                        (selected["batch_id"], selected["job_id"]),
                    ).fetchone()
                )
                job["batch_id"] = selected["batch_id"]
                return job
            except BaseException:
                self.connection.rollback()
                raise

    def finalize(self, batch_id: str, job_id: str, *, success: bool, retryable: bool, error: str | None) -> None:
        status = "succeeded" if success else ("pending" if retryable else "failed")
        with self.lock, self.connection:
            self.connection.execute(
                """
                UPDATE jobs
                SET status = ?, lease_expires_at = NULL, last_error = ?
                WHERE batch_id = ? AND job_id = ? AND status = 'leased'
                """,
                (status, None if success else (error or "sink failed"), batch_id, job_id),
            )

    @staticmethod
    def _job_from_row(row: sqlite3.Row) -> dict[str, Any]:
        return {
            "job_id": row["job_id"],
            "kind": row["kind"],
            "payload": json.loads(row["payload_json"]),
            "depends_on": json.loads(row["depends_json"]),
            "status": row["status"],
            "attempts": row["attempts"],
            "last_error": row["last_error"],
            "lease_expires_at": row["lease_expires_at"],
        }

    def get(self, batch_id: str) -> dict[str, Any] | None:
        with self.lock:
            now = self._now_ms()
            with self.connection:
                self._reap_expired_locked(now)
            batch = self.connection.execute(
                "SELECT batch_id FROM batches WHERE batch_id = ?", (batch_id,)
            ).fetchone()
            if batch is None:
                return None
            rows = self.connection.execute(
                "SELECT * FROM jobs WHERE batch_id = ? ORDER BY ordinal", (batch_id,)
            ).fetchall()
            jobs = [self._job_from_row(row) for row in rows]
            statuses = {job["status"] for job in jobs}
            if statuses == {"succeeded"}:
                status = "succeeded"
            elif statuses & {"failed", "blocked"}:
                status = "failed"
            else:
                status = "pending"
            return {"batch_id": batch_id, "status": status, "jobs": jobs}
