"""SQLite persistence for reading queue items.

The store owns every durable rule of the service: id ordering, unique URLs,
and the fixed ``queued`` / ``reading`` / ``done`` status vocabulary.  Tags are
persisted as a JSON array; because the API layer hands over tags that are
already trimmed, lower-cased, unique and sorted, the stored blob is itself
deterministic, which keeps rows byte-comparable and reads cheap.
"""

from __future__ import annotations

import json
import os
import sqlite3
import threading
from typing import Any, Iterable, Mapping

from .errors import DuplicateURL, UnknownItem

#: The only statuses the contract allows, in lifecycle order.
STATUSES: tuple[str, ...] = ("queued", "reading", "done")

#: Path used when the caller does not name a database file.
DEFAULT_DB_PATH = "readqueue.sqlite3"

_SCHEMA = """
CREATE TABLE IF NOT EXISTS items (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    title TEXT NOT NULL,
    url TEXT NOT NULL UNIQUE,
    status TEXT NOT NULL,
    tags TEXT NOT NULL DEFAULT '[]'
);
"""

_COLUMNS = ("id", "title", "url", "status", "tags")


def _encode_tags(tags: Iterable[str]) -> str:
    canonical = sorted({tag.strip().lower() for tag in tags})
    return json.dumps(canonical, ensure_ascii=False, sort_keys=True)


def ensure_parent_directory(path: str | os.PathLike[str]) -> str:
    """Create the parent directory of ``path`` when it is missing."""
    database = os.path.abspath(os.fspath(path))
    parent = os.path.dirname(database)
    if parent and not os.path.isdir(parent):
        os.makedirs(parent, exist_ok=True)
    return database


class ItemStore:
    """Thin, locking wrapper around the ``items`` table.

    A single connection is kept open for the lifetime of the store.  Handlers
    may run on different threads (the HTTP server is threaded), so every
    statement is serialised through a re-entrant lock; the specification
    explicitly drops any promise of concurrent-write isolation beyond that.
    """

    def __init__(self, path: str | os.PathLike[str] = DEFAULT_DB_PATH) -> None:
        self.path = os.fspath(path)
        if self.path != ":memory:":
            self.path = ensure_parent_directory(self.path)
        self._lock = threading.RLock()
        self._connection = sqlite3.connect(self.path, check_same_thread=False)
        self._connection.row_factory = sqlite3.Row
        with self._lock, self._connection:
            self._connection.executescript(_SCHEMA)

    # -- reading ---------------------------------------------------------

    def list(
        self, status: str | None = None, tag: str | None = None
    ) -> list[dict[str, Any]]:
        """Return items ordered by ascending id, optionally filtered.

        ``status`` is filtered in SQL; ``tag`` is compared against the stored
        tag list with an exact, case-insensitive match because tags live in a
        JSON column.
        """
        sql = f"SELECT {', '.join(_COLUMNS)} FROM items"
        params: list[Any] = []
        if status is not None:
            sql += " WHERE status = ?"
            params.append(status)
        sql += " ORDER BY id ASC"
        with self._lock:
            rows = self._connection.execute(sql, params).fetchall()
        items = [self._row_to_item(row) for row in rows]
        if tag is not None:
            wanted = tag.strip().lower()
            items = [item for item in items if wanted in item["tags"]]
        return items

    def get(self, item_id: int) -> dict[str, Any]:
        """Return one item, raising :class:`UnknownItem` when absent."""
        with self._lock:
            row = self._connection.execute(
                f"SELECT {', '.join(_COLUMNS)} FROM items WHERE id = ?", (item_id,)
            ).fetchone()
        if row is None:
            raise UnknownItem(item_id)
        return self._row_to_item(row)

    def count(self) -> int:
        """Return the number of stored items (used by tests)."""
        with self._lock:
            row = self._connection.execute("SELECT COUNT(*) AS total FROM items").fetchone()
        return int(row["total"])

    # -- writing ---------------------------------------------------------

    def create(self, title: str, url: str, tags: Iterable[str] = ()) -> dict[str, Any]:
        """Insert a new ``queued`` item and return it.

        Raises :class:`DuplicateURL` when the URL is already taken, leaving the
        table untouched.
        """
        with self._lock, self._connection:
            try:
                cursor = self._connection.execute(
                    "INSERT INTO items (title, url, status, tags) VALUES (?, ?, ?, ?)",
                    (title, url, "queued", _encode_tags(tags)),
                )
            except sqlite3.IntegrityError as exc:
                raise DuplicateURL(url) from exc
            new_id = int(cursor.lastrowid)
        return self.get(new_id)

    def update(self, item_id: int, changes: Mapping[str, Any]) -> dict[str, Any]:
        """Apply ``changes`` to an item and return the stored result.

        ``changes`` maps any subset of ``title``/``url``/``status``/``tags`` to
        already-validated values.  ``UnknownItem`` is raised for unknown ids and
        :class:`DuplicateURL` for URL collisions; in both cases the stored row
        is unchanged because the statement is rolled back.
        """
        assignments: list[str] = []
        params: list[Any] = []
        for column in ("title", "url", "status"):
            if column in changes:
                assignments.append(f"{column} = ?")
                params.append(changes[column])
        if "tags" in changes:
            assignments.append("tags = ?")
            params.append(_encode_tags(changes["tags"]))
        if not assignments:
            raise ValueError("update requires at least one field")
        params.append(item_id)
        sql = f"UPDATE items SET {', '.join(assignments)} WHERE id = ?"
        with self._lock, self._connection:
            try:
                cursor = self._connection.execute(sql, params)
            except sqlite3.IntegrityError as exc:
                raise DuplicateURL(changes["url"]) from exc
            if cursor.rowcount == 0:
                raise UnknownItem(item_id)
        return self.get(item_id)

    def delete(self, item_id: int) -> None:
        """Delete an item, raising :class:`UnknownItem` when absent."""
        with self._lock, self._connection:
            cursor = self._connection.execute("DELETE FROM items WHERE id = ?", (item_id,))
            if cursor.rowcount == 0:
                raise UnknownItem(item_id)

    # -- lifecycle -------------------------------------------------------

    def close(self) -> None:
        """Close the underlying SQLite connection."""
        with self._lock:
            self._connection.close()

    @staticmethod
    def _row_to_item(row: sqlite3.Row) -> dict[str, Any]:
        item = {column: row[column] for column in _COLUMNS}
        item["tags"] = json.loads(item["tags"])
        return item
