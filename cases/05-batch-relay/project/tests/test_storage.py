from __future__ import annotations

import tempfile
import time
import unittest
from pathlib import Path

from batchrelay.storage import BatchConflictError, Store


def batch(batch_id: str = "batch-1") -> dict:
    return {
        "batch_id": batch_id,
        "jobs": [
            {"job_id": "first", "kind": "first", "payload": {}, "depends_on": []},
            {"job_id": "second", "kind": "second", "payload": {}, "depends_on": ["first"]},
        ],
    }


class StorageTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.store = Store(Path(self.temp.name) / "relay.sqlite3")

    def tearDown(self) -> None:
        self.store.close()
        self.temp.cleanup()

    def test_submit_is_atomic_idempotent_and_conflicting(self) -> None:
        stored, created = self.store.submit(batch())
        self.assertTrue(created)
        same, created = self.store.submit(batch())
        self.assertFalse(created)
        self.assertEqual(same, stored)
        with self.assertRaises(BatchConflictError):
            self.store.submit({**batch(), "jobs": [dict(batch()["jobs"][0], payload={"changed": True})]})
        self.assertEqual(len(self.store.get("batch-1")["jobs"]), 2)

    def test_claims_dependencies_in_order_and_blocks_after_failure(self) -> None:
        self.store.submit(batch())
        first = self.store.claim_next(2)
        self.assertEqual(first["job_id"], "first")
        self.assertIsNone(self.store.claim_next(2))
        self.store.finalize("batch-1", "first", success=False, retryable=False, error="rejected")
        self.assertIsNone(self.store.claim_next(2))
        state = self.store.get("batch-1")
        self.assertEqual([job["status"] for job in state["jobs"]], ["failed", "blocked"])

    def test_expired_claim_is_reclaimed(self) -> None:
        one = {"batch_id": "lease", "jobs": [{"job_id": "job", "kind": "work", "payload": {}, "depends_on": []}]}
        self.store.submit(one)
        claimed = self.store.claim_next(1)
        self.assertEqual(claimed["attempts"], 1)
        time.sleep(1.05)
        self.assertEqual(self.store.get("lease")["jobs"][0]["status"], "pending")
        reclaimed = self.store.claim_next(2)
        self.assertEqual(reclaimed["attempts"], 2)
