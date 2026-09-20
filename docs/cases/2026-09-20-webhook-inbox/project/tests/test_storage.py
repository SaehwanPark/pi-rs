from __future__ import annotations

from pathlib import Path
import tempfile
import unittest

from webhookinbox.model import DeliveryConflictError
from webhookinbox.storage import Store


class StorageTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.database = Path(self.tempdir.name) / "state.sqlite3"
        self.store = Store(self.database)

    def tearDown(self) -> None:
        self.tempdir.cleanup()

    def test_admission_is_idempotent_but_conflicts_on_changed_content(self) -> None:
        first, created = self.store.admit("del-1", "release", {"version": "1"})
        self.assertTrue(created)
        duplicate, created = self.store.admit("del-1", "release", {"version": "1"})
        self.assertFalse(created)
        self.assertEqual(duplicate, first)
        with self.assertRaises(DeliveryConflictError):
            self.store.admit("del-1", "release", {"version": "2"})

    def test_expired_lease_is_reclaimable_and_failure_is_retryable(self) -> None:
        self.store.admit("del-1", "release", {})
        claimed = self.store.claim_next(1, now_ms=1000)
        self.assertIsNotNone(claimed)
        assert claimed is not None
        self.assertEqual(claimed.status, "leased")
        self.assertEqual(claimed.attempts, 1)
        self.assertEqual(self.store.get("del-1", now_ms=1500).status, "leased")  # type: ignore[union-attr]
        self.assertEqual(self.store.get("del-1", now_ms=2000).status, "pending")  # type: ignore[union-attr]
        reclaimed = self.store.claim_next(1, now_ms=2000)
        self.assertIsNotNone(reclaimed)
        assert reclaimed is not None
        self.assertEqual(reclaimed.attempts, 2)
        failed = self.store.finish("del-1", success=False, error="sink rejected")
        self.assertEqual(failed.status, "pending")
        self.assertEqual(failed.last_error, "sink rejected")

    def test_successful_finish_is_terminal(self) -> None:
        self.store.admit("del-1", "release", {})
        self.store.claim_next(30, now_ms=1000)
        delivered = self.store.finish("del-1", success=True)
        self.assertEqual(delivered.status, "delivered")
        self.assertIsNone(self.store.claim_next(30, now_ms=2000))


if __name__ == "__main__":
    unittest.main()
