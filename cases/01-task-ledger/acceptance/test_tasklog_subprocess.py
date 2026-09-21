"""Frozen black-box acceptance checks for the task-ledger toy project.

These tests intentionally live outside ``toy-project/tests``.  They invoke a
fresh Python interpreter for each command so the durable-process boundary in
``SPEC.md`` is exercised independently of the project's generated unit tests.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


TOY_PROJECT = Path(__file__).resolve().parents[1] / "toy-project"


class TasklogSubprocessAcceptance(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.cwd = Path(self.temp.name)
        self.env = os.environ.copy()
        python_path = str(TOY_PROJECT)
        existing = self.env.get("PYTHONPATH")
        if existing:
            python_path = os.pathsep.join((python_path, existing))
        self.env["PYTHONPATH"] = python_path
        self.env.pop("TASKLOG_PATH", None)

    def run_tasklog(self, *arguments: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, "-m", "tasklog", *arguments],
            cwd=self.cwd,
            env=self.env,
            text=True,
            capture_output=True,
            check=False,
        )

    def test_state_survives_a_complete_sequence_of_fresh_processes(self) -> None:
        add_one = self.run_tasklog("add", "write", "the", "docs")
        self.assertEqual(add_one.returncode, 0, add_one.stderr)
        self.assertIn("Added task 1", add_one.stdout)

        add_two = self.run_tasklog("add", "run", "the", "tests")
        self.assertEqual(add_two.returncode, 0, add_two.stderr)
        self.assertIn("Added task 2", add_two.stdout)

        listing = self.run_tasklog("list")
        self.assertEqual(listing.returncode, 0, listing.stderr)
        self.assertEqual(
            listing.stdout.splitlines(),
            ["   1 [ ] write the docs", "   2 [ ] run the tests", "2 open"],
        )

        done = self.run_tasklog("done", "1")
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("Completed task 1", done.stdout)

        all_tasks = self.run_tasklog("list", "--all")
        self.assertEqual(all_tasks.returncode, 0, all_tasks.stderr)
        self.assertEqual(
            all_tasks.stdout.splitlines(),
            ["   1 [x] write the docs", "   2 [ ] run the tests", "1 open, 1 done"],
        )

        remove = self.run_tasklog("remove", "2")
        self.assertEqual(remove.returncode, 0, remove.stderr)
        document = json.loads((self.cwd / ".tasklog.json").read_text(encoding="utf-8"))
        self.assertEqual(document["next_id"], 3)
        self.assertEqual(document["tasks"], [{"done": True, "id": 1, "text": "write the docs"}])

    def test_invalid_command_preserves_state_bytes(self) -> None:
        created = self.run_tasklog("add", "keep", "this")
        self.assertEqual(created.returncode, 0, created.stderr)
        state = self.cwd / ".tasklog.json"
        before = state.read_bytes()

        invalid = self.run_tasklog("done", "١")
        self.assertNotEqual(invalid.returncode, 0)
        self.assertIn("invalid id", invalid.stderr)
        self.assertEqual(state.read_bytes(), before)

    def test_global_state_option_and_fresh_list(self) -> None:
        fresh = self.run_tasklog("list")
        self.assertEqual(fresh.returncode, 0, fresh.stderr)
        self.assertIn("No open tasks", fresh.stdout)
        self.assertFalse((self.cwd / ".tasklog.json").exists())

        custom = self.cwd / "nested" / "ledger.json"
        custom.parent.mkdir()
        created = self.run_tasklog("--state", str(custom), "add", "custom path")
        self.assertEqual(created.returncode, 0, created.stderr)
        self.assertTrue(custom.exists())
        self.assertFalse((self.cwd / ".tasklog.json").exists())


if __name__ == "__main__":  # pragma: no cover
    unittest.main()
