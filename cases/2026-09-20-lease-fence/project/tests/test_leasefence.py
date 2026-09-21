from __future__ import annotations

import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

from leasefence.ids import ValidationError, normalize_pipeline
from leasefence.storage import Store
from leasefence.worker import _invoke_sink


def one_job(pipeline_id: str = "p") -> dict:
    return {
        "pipeline_id": pipeline_id,
        "jobs": [
            {
                "job_id": "source",
                "kind": "source",
                "payload": {"name": "source"},
                "depends_on": [],
                "input_refs": {},
                "collect": None,
            }
        ],
    }


def cascade(pipeline_id: str = "cascade") -> dict:
    return {
        "pipeline_id": pipeline_id,
        "jobs": [
            {
                "job_id": "source",
                "kind": "source",
                "payload": {},
                "depends_on": [],
                "input_refs": {},
                "collect": None,
            },
            {
                "job_id": "a",
                "kind": "part",
                "payload": {"name": "a"},
                "depends_on": ["source"],
                "input_refs": {"source": {"job_id": "source", "field": "artifact_id"}},
                "collect": None,
            },
            {
                "job_id": "b",
                "kind": "part",
                "payload": {"name": "b"},
                "depends_on": ["source"],
                "input_refs": {"source": {"job_id": "source", "field": "artifact_id"}},
                "collect": None,
            },
            {
                "job_id": "join",
                "kind": "barrier",
                "payload": {},
                "depends_on": ["a", "b"],
                "input_refs": {},
                "collect": {"field": "artifact_id", "as": "ids"},
            },
        ],
    }


class LeaseFenceTests(unittest.TestCase):
    def test_validation_rejects_cycle_and_normalizes(self) -> None:
        normalized = normalize_pipeline(one_job())
        self.assertEqual(normalized["pipeline_id"], "p")
        cycle = {
            "pipeline_id": "cycle",
            "jobs": [
                {"job_id": "a", "kind": "a", "payload": {}, "depends_on": ["b"], "input_refs": {}, "collect": None},
                {"job_id": "b", "kind": "b", "payload": {}, "depends_on": ["a"], "input_refs": {}, "collect": None},
            ],
        }
        with self.assertRaises(ValidationError):
            normalize_pipeline(cycle)

    def test_admission_is_idempotent_and_conflict_is_safe(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            store = Store(Path(directory) / "state.sqlite3")
            value = one_job("admit")
            first = store.admit(value)
            self.assertEqual(first.status, 202)
            repeat = store.admit(value)
            self.assertEqual(repeat.status, 200)
            changed = one_job("admit")
            changed["jobs"][0]["payload"]["name"] = "changed"
            conflict = store.admit(changed)
            self.assertEqual(conflict.status, 409)
            self.assertEqual(store.read("admit")["jobs"][0]["payload"]["name"], "source")

    def test_fencing_rejects_old_token_after_reclaim(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            store = Store(Path(directory) / "state.sqlite3")
            store.admit(one_job("fence"))
            first = store.claim_next(set(), lease_seconds=1)
            self.assertIsNotNone(first)
            second = None
            deadline = time.monotonic() + 4
            while time.monotonic() < deadline and second is None:
                second = store.claim_next(set(), lease_seconds=1)
                if second is None:
                    time.sleep(0.05)
            self.assertIsNotNone(second)
            self.assertNotEqual(first["lease_token"], second["lease_token"])
            self.assertFalse(store.finish_success(first, {"winner": "old"}))
            self.assertTrue(store.finish_success(second, {"winner": "new"}))
            result = store.read("fence")
            self.assertEqual(result["status"], "succeeded")
            self.assertEqual(result["jobs"][0]["output"], {"winner": "new"})
            self.assertEqual(result["jobs"][0]["attempts"], 2)

    def test_barrier_resolves_declared_order_only(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            store = Store(Path(directory) / "state.sqlite3")
            store.admit(cascade())
            source = store.claim_next(set(), 30)
            self.assertEqual(source["job_id"], "source")
            self.assertTrue(store.finish_success(source, {"artifact_id": "source-id", "private": "hidden"}))
            a = store.claim_next(set(), 30)
            self.assertEqual(a["job_id"], "a")
            self.assertEqual(store.resolve_inputs(a), {"source": "source-id"})
            self.assertTrue(store.finish_success(a, {"artifact_id": "a-id", "private": "a-secret"}))
            b = store.claim_next(set(), 30)
            self.assertEqual(b["job_id"], "b")
            self.assertTrue(store.finish_success(b, {"artifact_id": "b-id", "private": "b-secret"}))
            join = store.claim_next(set(), 30)
            self.assertEqual(join["fan_in"], {
                "as": "ids",
                "items": [{"job_id": "a", "value": "a-id"}, {"job_id": "b", "value": "b-id"}],
            })

    def test_terminal_failure_blocks_dependents(self) -> None:
        value = one_job("blocked")
        value["jobs"].append({
            "job_id": "child",
            "kind": "child",
            "payload": {},
            "depends_on": ["source"],
            "input_refs": {},
            "collect": None,
        })
        with tempfile.TemporaryDirectory() as directory:
            store = Store(Path(directory) / "state.sqlite3")
            store.admit(value)
            root = store.claim_next(set(), 30)
            self.assertTrue(store.finish_terminal(root, "terminal"))
            self.assertIsNone(store.claim_next(set(), 30))
            result = store.read("blocked")
            self.assertEqual([job["status"] for job in result["jobs"]], ["failed", "blocked"])

    def test_sink_uses_direct_argv_and_accepts_one_response(self) -> None:
        code = "import json,sys; value=json.loads(sys.stdin.readline()); print(json.dumps({'job_id':value['job_id'],'ok':True,'output':{'argv':sys.argv[1]}}))"
        outcome, output = _invoke_sink(sys.executable, ["-c", code, "-data"], {"job_id": "j"}, "j")
        self.assertEqual(outcome, "success")
        self.assertEqual(output, {"argv": "-data"})

    def test_help_commands_are_available(self) -> None:
        for arguments in (["--help"], ["serve", "--help"], ["worker", "--help"]):
            completed = subprocess.run(
                [sys.executable, "-m", "leasefence", *arguments],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertIn("usage:", completed.stdout.lower())


if __name__ == "__main__":
    unittest.main()
