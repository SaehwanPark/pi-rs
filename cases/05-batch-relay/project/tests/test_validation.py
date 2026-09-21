from __future__ import annotations

import unittest

from batchrelay.validation import ValidationError, validate_batch


def job(job_id: str, depends_on: list[str] | None = None) -> dict:
    return {
        "job_id": job_id,
        "kind": "work",
        "payload": {"job": job_id},
        "depends_on": depends_on or [],
    }


class ValidationTests(unittest.TestCase):
    def test_normalizes_text_and_accepts_a_dag(self) -> None:
        value = {
            "batch_id": " batch-1 ",
            "jobs": [job("root"), job("leaf", ["root"])],
        }
        normalized = validate_batch(value)
        self.assertEqual(normalized["batch_id"], "batch-1")
        self.assertEqual(normalized["jobs"][1]["depends_on"], ["root"])

    def test_rejects_unknown_duplicate_and_cyclic_dependencies(self) -> None:
        for jobs in (
            [job("root", ["missing"])],
            [job("root"), job("root")],
            [job("a", ["b"]), job("b", ["a"])],
        ):
            with self.subTest(jobs=jobs), self.assertRaises(ValidationError):
                validate_batch({"batch_id": "batch", "jobs": jobs})

    def test_rejects_unknown_fields_and_non_object_payload(self) -> None:
        with self.assertRaises(ValidationError):
            validate_batch({"batch_id": "batch", "jobs": [{**job("a"), "extra": True}]})
        with self.assertRaises(ValidationError):
            validate_batch({"batch_id": "batch", "jobs": [{**job("a"), "payload": []}]})
