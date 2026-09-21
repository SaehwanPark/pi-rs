import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from outbox.storage import Store
from outbox.worker import run_worker


class WorkerTests(unittest.TestCase):
    def test_direct_argv_sink_receives_and_acknowledges_event(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            db = root / "events.sqlite3"
            log = root / "received.jsonl"
            sink = root / "sink.py"
            sink.write_text(
                "import json, sys\n"
                "from pathlib import Path\n"
                "log = Path(sys.argv[sys.argv.index('--log') + 1])\n"
                "for line in sys.stdin:\n"
                "    item = json.loads(line)\n"
                "    log.write_text(json.dumps(item) + '\\n', encoding='utf-8')\n"
                "    print(json.dumps({'event_id': item['event_id'], 'ok': True}), flush=True)\n",
                encoding="utf-8",
            )
            store = Store(db)
            store.initialize()
            store.add({"event_id": "evt-1", "topic": "release", "payload": {"v": 1}})
            result = run_worker(str(db), sys.executable, [str(sink), "--log", str(log)])
            self.assertEqual(result, 0)
            self.assertEqual(store.get("evt-1")["status"], "delivered")
            received = json.loads(log.read_text(encoding="utf-8"))
            self.assertEqual(received["event_id"], "evt-1")

    def test_help_commands_are_executable(self) -> None:
        for arguments in (("--help",), ("serve", "--help"), ("worker", "--help")):
            result = subprocess.run(
                [sys.executable, "-m", "outbox", *arguments],
                text=True,
                capture_output=True,
                check=False,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
