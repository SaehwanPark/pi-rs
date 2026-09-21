from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

from leasereceipt.ids import ValidationError, delivery_key, normalize_pipeline
from leasereceipt.storage import Store
from leasereceipt.worker import _invoke_sink


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


class LeaseReceiptTests(unittest.TestCase):
    def test_validation_rejects_cycle_and_normalizes_delivery_identity(self) -> None:
        normalized = normalize_pipeline(one_job())
        self.assertEqual(normalized["pipeline_id"], "p")
        self.assertEqual(delivery_key("p", "source"), "p:source")
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
            self.assertEqual(first.pipeline["jobs"][0]["delivery_key"], "admit:source")
            self.assertIsNone(first.pipeline["jobs"][0]["receipt"])
            repeat = store.admit(json.loads(json.dumps(value)))
            self.assertEqual(repeat.status, 200)
            changed = one_job("admit")
            changed["jobs"][0]["payload"]["name"] = "changed"
            conflict = store.admit(changed)
            self.assertEqual(conflict.status, 409)
            self.assertEqual(store.read("admit")["jobs"][0]["payload"]["name"], "source")

    def test_claim_success_persists_output_and_receipt(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            store = Store(Path(directory) / "state.sqlite3")
            store.admit(one_job("finish"))
            job = store.claim_next(set(), 30)
            self.assertIsNotNone(job)
            self.assertEqual(job["delivery_key"], "finish:source")
            accepted = store.finish_success(job, {"artifact_id": "a-1"}, "receipt-finish-source")
            self.assertTrue(accepted)
            result = store.read("finish")
            self.assertEqual(result["status"], "succeeded")
            self.assertEqual(result["jobs"][0]["output"], {"artifact_id": "a-1"})
            self.assertEqual(result["jobs"][0]["receipt"], {
                "delivery_key": "finish:source",
                "receipt_id": "receipt-finish-source",
            })

    def test_stale_token_cannot_change_newer_receipt(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            store = Store(Path(directory) / "state.sqlite3")
            store.admit(one_job("fence"))
            first = store.claim_next(set(), 30)
            self.assertIsNotNone(first)
            stale = dict(first)
            accepted = store.finish_success(first, {"worker": "winner"}, "receipt-fence-source")
            self.assertTrue(accepted)
            self.assertFalse(store.finish_success(stale, {"worker": "stale"}, "receipt-stale"))
            result = store.read("fence")
            self.assertEqual(result["jobs"][0]["output"], {"worker": "winner"})
            self.assertEqual(result["jobs"][0]["receipt"]["receipt_id"], "receipt-fence-source")

    def test_expired_claim_is_reclaimed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            store = Store(Path(directory) / "state.sqlite3")
            store.admit(one_job("reclaim"))
            job = store.claim_next(set(), 1)
            self.assertIsNotNone(job)
            time.sleep(1.05)
            result = store.read("reclaim")
            self.assertEqual(result["jobs"][0]["status"], "pending")
            self.assertIn("reclaimed", result["jobs"][0]["last_error"])

    def test_declared_inputs_and_barrier_order(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            store = Store(Path(directory) / "state.sqlite3")
            store.admit(cascade())
            source = store.claim_next(set(), 30)
            self.assertTrue(store.finish_success(source, {"artifact_id": "source-out"}, "receipt-cascade-source"))
            part_a = store.claim_next({("cascade", "source")}, 30)
            self.assertEqual(store.resolve_inputs(part_a), {"source": "source-out"})
            self.assertTrue(store.finish_success(part_a, {"artifact_id": "a-out"}, "receipt-cascade-a"))
            part_b = store.claim_next({("cascade", "source"), ("cascade", "a")}, 30)
            self.assertTrue(store.finish_success(part_b, {"artifact_id": "b-out"}, "receipt-cascade-b"))
            join = store.claim_next({("cascade", "source"), ("cascade", "a"), ("cascade", "b")}, 30)
            self.assertEqual(join["fan_in"], {
                "as": "ids",
                "items": [{"job_id": "a", "value": "a-out"}, {"job_id": "b", "value": "b-out"}],
            })

    def test_sink_response_requires_matching_delivery_receipt(self) -> None:
        request_value = {
            "pipeline_id": "p",
            "job_id": "source",
            "delivery_key": "p:source",
        }
        code = (
            "import json,sys; value=json.loads(sys.stdin.readline()); "
            "print(json.dumps({'job_id':value['job_id'],'delivery_key':value['delivery_key'],"
            "'ok':True,'output':{'x':1},'receipt_id':'r-1'}))"
        )
        outcome, detail = _invoke_sink(
            sys.executable,
            ["-c", code],
            request_value,
            "source",
            "p:source",
        )
        self.assertEqual(outcome, "success")
        self.assertEqual(detail["receipt_id"], "r-1")

        mismatch_code = (
            "import json; print(json.dumps({'job_id':'source','delivery_key':'wrong',"
            "'ok':True,'output':{},'receipt_id':'r-1'}))"
        )
        outcome, _detail = _invoke_sink(
            sys.executable,
            ["-c", mismatch_code],
            request_value,
            "source",
            "p:source",
        )
        self.assertEqual(outcome, "retryable")

    def test_cli_help_is_available(self) -> None:
        for arguments in (["--help"], ["serve", "--help"], ["worker", "--help"]):
            completed = subprocess.run(
                [sys.executable, "-m", "leasereceipt", *arguments],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertIn("usage:", completed.stdout.lower())


if __name__ == "__main__":
    unittest.main()

