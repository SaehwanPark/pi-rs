"""Shared helpers for the test suite (no third-party dependencies)."""

from __future__ import annotations

import json
import os
import sys
import tempfile
import unittest
from typing import Any

# Allow ``import readqueue`` when unittest is pointed at ``tests`` directly.
PROJECT_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
if PROJECT_ROOT not in sys.path:
    sys.path.insert(0, PROJECT_ROOT)

from readqueue.api import Api  # noqa: E402
from readqueue.store import ItemStore  # noqa: E402


class StoreTestCase(unittest.TestCase):
    """Base class giving each test an isolated temporary SQLite database."""

    def setUp(self) -> None:
        self._directory = tempfile.TemporaryDirectory(prefix="readqueue-test-")
        self.addCleanup(self._directory.cleanup)
        self.db_path = os.path.join(self._directory.name, "data", "queue.sqlite3")
        self.store = ItemStore(self.db_path)
        self.addCleanup(self.store.close)
        self.api = Api(self.store)

    # -- assertions ------------------------------------------------------

    def assertError(self, response: Any, status: int, contains: str | None = None) -> None:
        """Assert ``response`` is an error payload with the expected status."""
        self.assertEqual(response.status, status, msg=getattr(response, "payload", None))
        self.assertIsInstance(response.payload, dict)
        self.assertIn("error", response.payload)
        self.assertIsInstance(response.payload["error"], str)
        self.assertTrue(response.payload["error"], "error message must not be empty")
        if contains is not None:
            self.assertIn(contains, response.payload["error"])

    def json_of(self, response: Any) -> Any:
        """Decode a response body the way an HTTP client would see it."""
        return json.loads(response.body().decode("utf-8")) if response.has_body else None
