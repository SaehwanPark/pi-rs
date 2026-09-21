from __future__ import annotations

import copy
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from leasecascade.ids import ValidationError, normalize_pipeline
from leasecascade.storage import Store


def pipeline(pipeline_id: str = "p-1") -> dict:
    return {
        "pipeline_id": pipeline_id,
        "jobs": [
            {
                "job_id": "source",
                "kind": "source",
                "payload": {"batch": "b-1"},
                "depends_on": [],
                "input_refs": {},
                "collect": None,
            },
            {
                "job_id": "part-a",
                "kind": "part",
                "payload": {"name": "a"},
                "depends_on": ["source"],
                "input_refs": {"source_id": {"job_id": "source", "field": "artifact_id"}},
                "collect": None,
            },
            {
                "job_id": "part-b",
                "kind": "part",
                "payload": {"name": "b"},
                "depends_on": ["source"],
                "input_refs": {"source_id": {"job_id": "source", "field": "artifact_id"}},
                "collect": None,
            },
            {
                "job_id": "barrier",
                "kind": "barrier",
                "payload": {"name": "join"},
                "depends_on": ["part-a", "part-b"],
                "input_refs": {},
                "collect": {"field": "artifact_id", "as": "artifacts"},
            },
            {
                "job_id": "publish",
                "kind": "publish",
                "payload": {},
                "depends_on": ["barrier"],
                "input_refs": {"joined": {"job_id": "barrier", "field": "joined"}},
                "collect": None,
            },
        ],
    }


class ValidationTests(unittest.TestCase):
    def test_normalizes_barrier_and_rejects_non_barrier_collection(self) -> None:
        document = normalize_pipeline(pipeline())
        self.assertEqual(document["jobs"][3]["collect"]["as"], "artifacts")
        invalid = pipeline()
        invalid["jobs"][1]["collect"] = {"field": "artifact_id", "as": "wrong"}
        with self.assertRaises(ValidationError):
            normalize_pipeline(invalid)

    def test_rejects_cycles_bad_reference_and_unknown_fields(self) -> None:
        invalid = pipeline()
        invalid["jobs"][0]["depends_on"] = ["publish"]
        with self.assertRaises(ValidationError):
            normalize_pipeline(invalid)
        invalid = pipeline()
        invalid["jobs"][3]["collect"] = {"field": "nested.value", "as": "items"}
        with self.assertRaises(ValidationError):
            normalize_pipeline(invalid)
        invalid = pipeline()
        invalid["extra"] = True
        with self.assertRaises(ValidationError):
            normalize_pipeline(invalid)


class StorageTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="leasecascade-test-")
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
        self.assertEqual(len(self.store.read("p-1")["jobs"]), 5)

    def test_claim_resolve_and_ordered_fan_in(self) -> None:
        self.store.admit(pipeline())
        excluded: set[tuple[str, str]] = set()
        source = self.store.claim_next(excluded, 2)
        self.assertEqual(source["job_id"], "source")
        excluded.add(("p-1", "source"))
        self.store.finish_success(source, {"artifact_id": "source-1", "private": "kept"})
        part_a = self.store.claim_next(excluded, 2)
        self.assertEqual(self.store.resolve_inputs(part_a), {"source_id": "source-1"})
        excluded.add(("p-1", "part-a"))
        self.store.finish_success(part_a, {"artifact_id": "a-1", "private": "hidden-a"})
        part_b = self.store.claim_next(excluded, 2)
        excluded.add(("p-1", "part-b"))
        self.store.finish_success(part_b, {"artifact_id": "b-1", "private": "hidden-b"})
        barrier = self.store.claim_next(excluded, 2)
        self.assertEqual(
            barrier["fan_in"],
            {
                "as": "artifacts",
                "items": [{"job_id": "part-a", "value": "a-1"}, {"job_id": "part-b", "value": "b-1"}],
            },
        )
        self.store.finish_success(barrier, {"joined": ["a-1", "b-1"]})

    def test_missing_collected_field_fails_barrier_without_attempt(self) -> None:
        self.store.admit(pipeline("missing"))
        excluded: set[tuple[str, str]] = set()
        source = self.store.claim_next(excluded, 2)
        excluded.add(("missing", "source"))
        self.store.finish_success(source, {"artifact_id": "source-1"})
        part_a = self.store.claim_next(excluded, 2)
        excluded.add(("missing", "part-a"))
        self.store.finish_success(part_a, {"artifact_id": "a-1"})
        part_b = self.store.claim_next(excluded, 2)
        excluded.add(("missing", "part-b"))
        self.store.finish_success(part_b, {"private": "no-artifact"})
        local = self.store.claim_next(excluded, 2)
        self.assertFalse(local["claimed"])
        self.assertEqual(self.store.read("missing")["jobs"][3]["status"], "failed")
        self.assertEqual(self.store.read("missing")["jobs"][3]["attempts"], 0)
        self.assertEqual(self.store.read("missing")["jobs"][4]["status"], "blocked")

    def test_retryable_state_and_terminal_blocking(self) -> None:
        self.store.admit(pipeline("retry"))
        job = self.store.claim_next(set(), 2)
        self.store.finish_retryable(job, "temporary")
        self.assertEqual(self.store.read("retry")["jobs"][0]["status"], "pending")

        self.store.admit(pipeline("failed"))
        root = self.store.claim_next({("retry", "source")}, 2)
        self.store.finish_terminal(root, "permanent")
        statuses = [item["status"] for item in self.store.read("failed")["jobs"]]
        self.assertEqual(statuses, ["failed", "blocked", "blocked", "blocked", "blocked"])

    def test_help_commands(self) -> None:
        project_root = Path(__file__).resolve().parents[1]
        for args in ([], ["serve"], ["worker"]):
            result = subprocess.run(
                [sys.executable, "-m", "leasecascade", *args, "--help"],
                cwd=project_root,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("help", (result.stdout + result.stderr).lower())


if __name__ == "__main__":
    unittest.main()
