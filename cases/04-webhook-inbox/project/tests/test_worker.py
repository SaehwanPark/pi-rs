from __future__ import annotations

from pathlib import Path
import sys
import tempfile
import unittest

from webhookinbox.storage import Store
from webhookinbox.worker import run_worker


SUCCESS_SINK = """
import json
import sys
request = json.loads(sys.stdin.readline())
print(json.dumps({'delivery_id': request['delivery_id'], 'ok': True}))
"""

FAILURE_SINK = """
import json
import sys
request = json.loads(sys.stdin.readline())
print(json.dumps({'delivery_id': request['delivery_id'], 'ok': False}))
"""


class WorkerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.root = Path(self.tempdir.name)
        self.database = self.root / "state.sqlite3"
        self.sink = self.root / "sink.py"

    def tearDown(self) -> None:
        self.tempdir.cleanup()

    def _write_sink(self, source: str) -> None:
        self.sink.write_text(source, encoding="utf-8")

    def test_direct_argv_sink_success_marks_delivery_delivered(self) -> None:
        Store(self.database).admit("del-1", "release", {"version": "1"})
        self._write_sink(SUCCESS_SINK)
        self.assertEqual(run_worker(str(self.database), sys.executable, [str(self.sink)], 30), 0)
        delivery = Store(self.database).get("del-1")
        self.assertIsNotNone(delivery)
        assert delivery is not None
        self.assertEqual(delivery.status, "delivered")
        self.assertEqual(delivery.attempts, 1)

    def test_failed_sink_is_recorded_once_and_worker_is_bounded(self) -> None:
        Store(self.database).admit("del-1", "release", {})
        self._write_sink(FAILURE_SINK)
        self.assertEqual(run_worker(str(self.database), sys.executable, [str(self.sink)], 30), 1)
        delivery = Store(self.database).get("del-1")
        self.assertIsNotNone(delivery)
        assert delivery is not None
        self.assertEqual(delivery.status, "pending")
        self.assertEqual(delivery.attempts, 1)
        self.assertTrue(delivery.last_error)


if __name__ == "__main__":
    unittest.main()
