from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from artifactpipe.ids import ValidationError, normalize_pipeline
from artifactpipe.storage import Store


def pipeline(pipeline_id: str = "p-1") -> dict:
    return {
        "pipeline_id": pipeline_id,
        "jobs": [
            {"job_id": "build", "kind": "build", "payload": {}, "depends_on": [], "input_refs": {}},
            {
                "job_id": "publish",
                "kind": "publish",
                "payload": {},
                "depends_on": ["build"],
                "input_refs": {"artifact_id": {"job_id": "build", "field": "artifact_id"}},
            },
        ],
    }


class ValidationTests(unittest.TestCase):
    def test_normalizes_and_rejects_undeclared_reference(self) -> None:
        document = normalize_pipeline(pipeline())
        self.assertEqual(document["pipeline_id"], "p-1")
        self.assertEqual(document["jobs"][1]["input_refs"]["artifact_id"]["field"], "artifact_id")
        invalid = pipeline()
        invalid["jobs"][1]["depends_on"] = []
        with self.assertRaises(ValidationError):
            normalize_pipeline(invalid)

    def test_rejects_cycles_and_unknown_fields(self) -> None:
        invalid = pipeline()
        invalid["jobs"][0]["depends_on"] = ["publish"]
        with self.assertRaises(ValidationError):
            normalize_pipeline(invalid)
        invalid = pipeline()
        invalid["extra"] = True
        with self.assertRaises(ValidationError):
            normalize_pipeline(invalid)


class StorageTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="artifactpipe-test-")
        self.db = Path(self.temp.name) / "nested" / "pipeline.sqlite3"
        self.store = Store(self.db)

    def tearDown(self) -> None:
        self.temp.cleanup()

    def test_atomic_idempotent_and_conflicting_admission(self) -> None:
        first = self.store.admit(pipeline())
        self.assertEqual(first.status, 202)
        again = self.store.admit(pipeline())
        self.assertEqual(again.status, 200)
        changed = pipeline()
        changed["jobs"][0]["payload"] = {"version": 2}
        self.assertEqual(self.store.admit(changed).status, 409)
        self.assertEqual(len(self.store.read("p-1")["jobs"]), 2)

    def test_claim_resolve_and_successful_output(self) -> None:
        self.store.admit(pipeline())
        first = self.store.claim_next(set(), 2)
        self.assertIsNotNone(first)
        self.assertEqual(first["job_id"], "build")
        self.store.finish_success(first, {"artifact_id": "a-1", "private": "kept"})
        second = self.store.claim_next({("p-1", "build")}, 2)
        self.assertEqual(self.store.resolve_inputs(second), {"artifact_id": "a-1"})
        self.store.finish_success(second, {"receipt": "r-1"})
        result = self.store.read("p-1")
        self.assertEqual(result["status"], "succeeded")
        self.assertEqual(result["jobs"][0]["output"]["private"], "kept")

    def test_retryable_failure_stays_pending_and_terminal_failure_blocks(self) -> None:
        self.store.admit(pipeline("retry"))
        job = self.store.claim_next(set(), 2)
        self.store.finish_retryable(job, "temporary")
        self.assertEqual(self.store.read("retry")["jobs"][0]["status"], "pending")

        self.store.admit(pipeline("failed"))
        root = self.store.claim_next({("retry", "build"), ("retry", "publish")}, 2)
        self.store.finish_terminal(root, "permanent")
        statuses = [item["status"] for item in self.store.read("failed")["jobs"]]
        self.assertEqual(statuses, ["failed", "blocked"])

    def test_help_commands(self) -> None:
        for args in ([], ["serve"], ["worker"]):
            result = subprocess.run(
                [sys.executable, "-m", "artifactpipe", *args, "--help"],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("help", (result.stdout + result.stderr).lower())


if __name__ == "__main__":
    unittest.main()
