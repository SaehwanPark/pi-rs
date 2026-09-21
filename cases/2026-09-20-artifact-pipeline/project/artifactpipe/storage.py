"""SQLite persistence and explicit pipeline/job state transitions."""

from __future__ import annotations

import json
import sqlite3
import time
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from .ids import content_hash, normalize_pipeline


class PipelineNotFound(KeyError):
    """The requested pipeline does not exist."""


class InputResolutionError(ValueError):
    """A declared input could not be resolved from a successful dependency."""


@dataclass(frozen=True)
class Admission:
    status: int
    pipeline: dict[str, Any]


class Store:
    def __init__(self, path: str | Path) -> None:
        self.path = Path(path)
        self._initialize()

    def _connect(self) -> sqlite3.Connection:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        connection = sqlite3.connect(self.path, timeout=10)
        connection.row_factory = sqlite3.Row
        connection.execute("PRAGMA foreign_keys = ON")
        connection.execute("PRAGMA busy_timeout = 10000")
        return connection

    @contextmanager
    def _session(self):
        connection = self._connect()
        try:
            yield connection
        finally:
            connection.close()

    def _initialize(self) -> None:
        with self._session() as connection:
            connection.executescript(
                """
                CREATE TABLE IF NOT EXISTS pipelines (
                    pipeline_seq INTEGER PRIMARY KEY AUTOINCREMENT,
                    pipeline_id TEXT NOT NULL UNIQUE,
                    content_hash TEXT NOT NULL,
                    created_at INTEGER NOT NULL
                );
                CREATE TABLE IF NOT EXISTS jobs (
                    pipeline_id TEXT NOT NULL,
                    job_id TEXT NOT NULL,
                    position INTEGER NOT NULL,
                    kind TEXT NOT NULL,
                    payload TEXT NOT NULL,
                    depends_on TEXT NOT NULL,
                    input_refs TEXT NOT NULL,
                    status TEXT NOT NULL,
                    attempts INTEGER NOT NULL DEFAULT 0,
                    last_error TEXT,
                    lease_expires_at INTEGER,
                    output TEXT,
                    PRIMARY KEY (pipeline_id, job_id),
                    FOREIGN KEY (pipeline_id) REFERENCES pipelines(pipeline_id) ON DELETE CASCADE
                );
                CREATE INDEX IF NOT EXISTS jobs_status_idx ON jobs(status);
                """
            )

    def admit(self, document: Any) -> Admission:
        normalized = normalize_pipeline(document)
        digest = content_hash(normalized)
        with self._session() as connection:
            connection.execute("BEGIN IMMEDIATE")
            existing = connection.execute(
                "SELECT content_hash FROM pipelines WHERE pipeline_id = ?",
                (normalized["pipeline_id"],),
            ).fetchone()
            if existing is not None:
                connection.commit()
                status = 200 if existing["content_hash"] == digest else 409
                return Admission(status, self.read(normalized["pipeline_id"]))

            connection.execute(
                "INSERT INTO pipelines (pipeline_id, content_hash, created_at) VALUES (?, ?, ?)",
                (normalized["pipeline_id"], digest, _now_ms()),
            )
            for position, job in enumerate(normalized["jobs"]):
                connection.execute(
                    """
                    INSERT INTO jobs
                    (pipeline_id, job_id, position, kind, payload, depends_on, input_refs, status)
                    VALUES (?, ?, ?, ?, ?, ?, ?, 'pending')
                    """,
                    (
                        normalized["pipeline_id"],
                        job["job_id"],
                        position,
                        job["kind"],
                        _dump(job["payload"]),
                        _dump(job["depends_on"]),
                        _dump(job["input_refs"]),
                    ),
                )
            connection.commit()
        return Admission(202, self.read(normalized["pipeline_id"]))

    def read(self, pipeline_id: str) -> dict[str, Any]:
        with self._session() as connection:
            connection.execute("BEGIN IMMEDIATE")
            self._refresh(connection)
            pipeline = connection.execute(
                "SELECT pipeline_id FROM pipelines WHERE pipeline_id = ?", (pipeline_id,)
            ).fetchone()
            if pipeline is None:
                connection.commit()
                raise PipelineNotFound(pipeline_id)
            jobs = connection.execute(
                """
                SELECT job_id, kind, payload, depends_on, input_refs, status, attempts,
                       last_error, lease_expires_at, output
                FROM jobs WHERE pipeline_id = ? ORDER BY position
                """,
                (pipeline_id,),
            ).fetchall()
            result = self._pipeline_result(pipeline_id, jobs)
            connection.commit()
            return result

    def claim_next(self, excluded: set[tuple[str, str]], lease_seconds: int) -> dict[str, Any] | None:
        with self._session() as connection:
            connection.execute("BEGIN IMMEDIATE")
            self._refresh(connection)
            rows = connection.execute(
                """
                SELECT j.pipeline_id, j.job_id, j.position, j.kind, j.payload, j.depends_on,
                       j.input_refs, j.attempts, p.pipeline_seq
                FROM jobs AS j JOIN pipelines AS p ON p.pipeline_id = j.pipeline_id
                WHERE j.status = 'pending'
                ORDER BY p.pipeline_seq, j.position
                """
            ).fetchall()
            for row in rows:
                key = (row["pipeline_id"], row["job_id"])
                if key in excluded:
                    continue
                dependencies = json.loads(row["depends_on"])
                statuses = {
                    dependency["job_id"]: dependency["status"]
                    for dependency in connection.execute(
                        "SELECT job_id, status FROM jobs WHERE pipeline_id = ?", (row["pipeline_id"],)
                    ).fetchall()
                }
                if not all(statuses.get(dependency) == "succeeded" for dependency in dependencies):
                    continue
                expires = _now_ms() + lease_seconds * 1000
                updated = connection.execute(
                    """
                    UPDATE jobs SET status = 'leased', lease_expires_at = ?, attempts = attempts + 1
                    WHERE pipeline_id = ? AND job_id = ? AND status = 'pending'
                    """,
                    (expires, row["pipeline_id"], row["job_id"]),
                ).rowcount
                if updated != 1:
                    continue
                connection.commit()
                return {
                    "pipeline_id": row["pipeline_id"],
                    "job_id": row["job_id"],
                    "kind": row["kind"],
                    "payload": json.loads(row["payload"]),
                    "depends_on": dependencies,
                    "input_refs": json.loads(row["input_refs"]),
                    "attempts": int(row["attempts"]) + 1,
                    "lease_expires_at": expires,
                }
            connection.commit()
            return None

    def resolve_inputs(self, job: dict[str, Any]) -> dict[str, Any]:
        if not job["input_refs"]:
            return {}
        with self._session() as connection:
            rows = connection.execute(
                "SELECT job_id, status, output FROM jobs WHERE pipeline_id = ?",
                (job["pipeline_id"],),
            ).fetchall()
        by_id = {row["job_id"]: row for row in rows}
        resolved: dict[str, Any] = {}
        for input_name, reference in job["input_refs"].items():
            source = by_id.get(reference["job_id"])
            if source is None or source["status"] != "succeeded" or source["output"] is None:
                raise InputResolutionError(f"input {input_name} has no successful dependency output")
            output = json.loads(source["output"])
            if not isinstance(output, dict) or reference["field"] not in output:
                raise InputResolutionError(
                    f"input {input_name} is missing field {reference['field']} from {reference['job_id']}"
                )
            resolved[input_name] = output[reference["field"]]
        return resolved

    def finish_success(self, job: dict[str, Any], output: dict[str, Any]) -> None:
        self._finish(job, "succeeded", None, output)

    def finish_retryable(self, job: dict[str, Any], message: str) -> None:
        self._finish(job, "pending", message, None)

    def finish_terminal(self, job: dict[str, Any], message: str) -> None:
        self._finish(job, "failed", message, None)

    def _finish(self, job: dict[str, Any], status: str, message: str | None, output: dict[str, Any] | None) -> None:
        with self._session() as connection:
            connection.execute("BEGIN IMMEDIATE")
            connection.execute(
                """
                UPDATE jobs SET status = ?, last_error = ?, lease_expires_at = NULL,
                    output = CASE WHEN ? = 'succeeded' THEN ? ELSE output END
                WHERE pipeline_id = ? AND job_id = ? AND status = 'leased'
                """,
                (status, message, status, _dump(output) if output is not None else None, job["pipeline_id"], job["job_id"]),
            )
            self._refresh(connection)
            connection.commit()

    def _refresh(self, connection: sqlite3.Connection) -> None:
        connection.execute(
            """
            UPDATE jobs SET status = 'pending', lease_expires_at = NULL,
                last_error = 'lease expired and was reclaimed'
            WHERE status = 'leased' AND lease_expires_at IS NOT NULL AND lease_expires_at <= ?
            """,
            (_now_ms(),),
        )
        changed = True
        while changed:
            changed = False
            rows = connection.execute(
                "SELECT pipeline_id, job_id, depends_on FROM jobs WHERE status = 'pending'"
            ).fetchall()
            for row in rows:
                dependencies = json.loads(row["depends_on"])
                if not dependencies:
                    continue
                statuses = {
                    dependency["job_id"]: dependency["status"]
                    for dependency in connection.execute(
                        "SELECT job_id, status FROM jobs WHERE pipeline_id = ?", (row["pipeline_id"],)
                    ).fetchall()
                }
                if any(statuses.get(dependency) in {"failed", "blocked"} for dependency in dependencies):
                    connection.execute(
                        "UPDATE jobs SET status = 'blocked', lease_expires_at = NULL WHERE pipeline_id = ? AND job_id = ?",
                        (row["pipeline_id"], row["job_id"]),
                    )
                    changed = True

    def _pipeline_result(self, pipeline_id: str, jobs: list[sqlite3.Row]) -> dict[str, Any]:
        statuses = [job["status"] for job in jobs]
        if statuses and all(status == "succeeded" for status in statuses):
            status = "succeeded"
        elif any(item in {"failed", "blocked"} for item in statuses):
            status = "failed"
        else:
            status = "pending"
        return {
            "pipeline_id": pipeline_id,
            "status": status,
            "jobs": [
                {
                    "job_id": job["job_id"],
                    "kind": job["kind"],
                    "payload": json.loads(job["payload"]),
                    "depends_on": json.loads(job["depends_on"]),
                    "input_refs": json.loads(job["input_refs"]),
                    "status": job["status"],
                    "attempts": int(job["attempts"]),
                    "last_error": job["last_error"],
                    "lease_expires_at": job["lease_expires_at"],
                    "output": json.loads(job["output"]) if job["output"] is not None else None,
                }
                for job in jobs
            ],
        }


def _dump(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def _now_ms() -> int:
    return int(time.time() * 1000)
