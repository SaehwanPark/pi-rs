import tempfile
import unittest
from pathlib import Path

from outbox.storage import Store


class StorageTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.store = Store(Path(self.temp.name) / "nested" / "events.sqlite3")
        self.store.initialize()

    def tearDown(self) -> None:
        self.temp.cleanup()

    def test_idempotency_and_conflict(self) -> None:
        event = {"event_id": "evt-1", "topic": "release", "payload": {"v": 1}}
        outcome, created = self.store.add(event)
        self.assertEqual(outcome, "created")
        outcome, duplicate = self.store.add({"event_id": "evt-1", "topic": "release", "payload": {"v": 1}})
        self.assertEqual(outcome, "duplicate")
        self.assertEqual(duplicate, created)
        outcome, _ = self.store.add({"event_id": "evt-1", "topic": "other", "payload": {"v": 1}})
        self.assertEqual(outcome, "conflict")

    def test_attempts_and_retry_status_are_durable(self) -> None:
        self.store.add({"event_id": "evt-1", "topic": "release", "payload": {}})
        self.store.record_attempt("evt-1", False, "planned failure")
        failed = self.store.get("evt-1")
        self.assertEqual(failed["status"], "pending")
        self.assertEqual(failed["attempts"], 1)
        self.assertEqual(failed["last_error"], "planned failure")
        self.store.record_attempt("evt-1", True)
        delivered = self.store.get("evt-1")
        self.assertEqual(delivered["status"], "delivered")
        self.assertEqual(delivered["attempts"], 2)
        self.assertIsNone(delivered["last_error"])
