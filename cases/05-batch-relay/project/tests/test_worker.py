from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

from batchrelay.storage import Store
from batchrelay.worker import invoke_sink


class WorkerTests(unittest.TestCase):
    def test_direct_sink_success_and_retryable_failure(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            script = Path(directory) / "sink.py"
            script.write_text(
                "import json, sys\n"
                "job=json.loads(sys.stdin.readline())\n"
                "print(json.dumps({'job_id': job['job_id'], 'ok': True}))\n",
                encoding="utf-8",
            )
            command = [sys.executable, str(script)]
            job = {
                "batch_id": "batch",
                "job_id": "job",
                "kind": "work",
                "payload": {},
                "depends_on": [],
            }
            self.assertEqual(invoke_sink(command, job), (True, False, None))

    def test_mismatched_response_is_retryable_failure(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            script = Path(directory) / "sink.py"
            script.write_text(
                "import json\n"
                "print(json.dumps({'job_id': 'other', 'ok': True}))\n",
                encoding="utf-8",
            )
            result = invoke_sink(
                [sys.executable, str(script)],
                {"batch_id": "batch", "job_id": "job", "kind": "work", "payload": {}, "depends_on": []},
            )
            self.assertEqual(result[0], False)
            self.assertTrue(result[1])
            self.assertIn("did not match", result[2])
