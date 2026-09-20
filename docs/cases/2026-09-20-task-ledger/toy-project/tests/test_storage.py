"""Persistence: path resolution, atomic writes, corruption refusal, file shape."""

from __future__ import annotations

import json
import os
import stat
import unittest
from pathlib import Path
from unittest import mock

from tasklog.model import Ledger, Task
from tasklog.storage import (
    DEFAULT_FILENAME,
    ENV_PATH,
    StorageError,
    load_ledger,
    resolve_path,
    save_ledger,
)

from support import TempDirCase


class SampleLedgers:
    """A few ledger values shared by several storage tests."""

    @staticmethod
    def two_tasks_one_done() -> Ledger:
        ledger, _ = Ledger.empty().add("one")
        ledger, _ = ledger.add("two")
        ledger, _ = ledger.mark_done(1)
        return ledger


class ResolvePathTests(TempDirCase):
    def test_default_is_cwd_file(self):
        with mock.patch.object(Path, "cwd", return_value=self.base):
            self.assertEqual(resolve_path(), self.state)

    def test_environment_override(self):
        custom = self.base / "elsewhere" / "tasks.json"
        (self.base / "elsewhere").mkdir()
        with mock.patch.dict(os.environ, {ENV_PATH: str(custom)}):
            self.assertEqual(resolve_path(), custom)

    def test_explicit_argument_beats_environment(self):
        with mock.patch.dict(os.environ, {ENV_PATH: str(self.base / "env.json")}):
            chosen = resolve_path(self.base / "explicit.json")
        self.assertEqual(chosen, self.base / "explicit.json")

    def test_empty_environment_is_ignored(self):
        with mock.patch.dict(os.environ, {ENV_PATH: "  "}):
            with mock.patch.object(Path, "cwd", return_value=self.base):
                self.assertEqual(resolve_path(), self.base / DEFAULT_FILENAME)

    def test_base_argument_without_environment(self):
        self.assertEqual(resolve_path(base=self.base), self.state)


class SaveLoadTests(TempDirCase):
    def test_load_missing_file_is_empty_ledger(self):
        self.assertEqual(load_ledger(self.state), Ledger.empty())
        self.assertFalse(self.state.exists(), "load must not create the file")

    def test_save_then_load_round_trip_across_calls(self):
        ledger = SampleLedgers.two_tasks_one_done()
        save_ledger(self.state, ledger)
        self.assertEqual(load_ledger(self.state), ledger)

    def test_file_is_human_readable_json(self):
        save_ledger(self.state, SampleLedgers.two_tasks_one_done())
        text = self.state.read_text(encoding="utf-8")
        document = json.loads(text)
        self.assertEqual(document["version"], 1)
        self.assertIn("\n", text, "state file must be indented for humans")
        self.assertTrue(text.endswith("\n"))
        # Deterministic: keys sorted, tasks sorted, so VCS diffs stay stable.
        self.assertEqual(list(document["tasks"][0]), ["done", "id", "text"])
        self.assertEqual([task["id"] for task in document["tasks"]], [1, 2])

    def test_save_overwrites_completely(self):
        save_ledger(self.state, SampleLedgers.two_tasks_one_done())
        save_ledger(self.state, Ledger.empty())
        self.assertEqual(load_ledger(self.state), Ledger.empty())
        self.assertEqual(self.temp_files(), [])

    def test_unicode_survives_round_trip(self):
        ledger, _ = Ledger.empty().add("café — 日本語 task")
        save_ledger(self.state, ledger)
        self.assertEqual(load_ledger(self.state).get(1).text, "café — 日本語 task")

    def test_empty_file_reads_as_empty_ledger(self):
        self.write_state("   \n")
        self.assertEqual(load_ledger(self.state), Ledger.empty())

    def test_save_missing_parent_directory_reports_clearly(self):
        target = self.base / "nope" / DEFAULT_FILENAME
        with self.assertRaises(StorageError) as caught:
            save_ledger(target, Ledger.empty())
        self.assertIn("does not exist", str(caught.exception))
        self.assertFalse(target.parent.exists())


class LoadCorruptionTests(TempDirCase):
    def test_invalid_json_raises_storage_error(self):
        self.write_state("{not json")
        with self.assertRaises(StorageError) as caught:
            load_ledger(self.state)
        self.assertIn(str(self.state), str(caught.exception))

    def test_corrupt_document_reports_file_path(self):
        self.write_state({"version": 1, "tasks": [{"id": 0, "text": "x", "done": False}]})
        with self.assertRaises(StorageError) as caught:
            load_ledger(self.state)
        self.assertIn(str(self.state), str(caught.exception))

    def test_state_path_as_directory_reports_clearly(self):
        with self.assertRaises(StorageError):
            load_ledger(self.base)

    def test_unreadable_file_reports_clearly(self):
        # Simulate an unreadable file without relying on POSIX permissions.
        with mock.patch.object(
            Path, "read_text", side_effect=OSError("Permission denied")
        ):
            with self.assertRaises(StorageError):
                load_ledger(self.state)


class AtomicityTests(TempDirCase):
    def test_serialisation_failure_leaves_file_untouched(self):
        good = SampleLedgers.two_tasks_one_done()
        save_ledger(self.state, good)
        before = self.state_bytes()
        broken = Ledger(next_id=1, tasks=(Task(1, "ok"),))
        # Force json.dumps to fail *after* load succeeded: a ledger holding a
        # non-serialisable value cannot arise from JSON, so patch as_document.
        with mock.patch.object(
            Ledger, "as_document", side_effect=TypeError("nope")
        ):
            with self.assertRaises(TypeError):
                save_ledger(self.state, broken)
        self.assertEqual(self.state_bytes(), before)
        self.assertEqual(self.temp_files(), [])

    def test_write_failure_keeps_previous_content_and_no_temp(self):
        good = SampleLedgers.two_tasks_one_done()
        save_ledger(self.state, good)
        before = self.state_bytes()

        real_replace = os.replace

        def crashing_replace(src, dst, **kwargs):
            # The temp file has been fully written when os.replace is called:
            # fail here, as a process crash or locked file would.
            raise OSError("simulated failure mid-write")

        with mock.patch("tasklog.storage.os.replace", side_effect=crashing_replace):
            with self.assertRaises(StorageError):
                save_ledger(self.state, Ledger.empty().add("new"))
        self.assertEqual(self.state_bytes(), before)
        self.assertEqual(load_ledger(self.state), good)
        self.assertEqual(self.temp_files(), [])
        self.assertIs(real_replace, os.replace)


if __name__ == "__main__":  # pragma: no cover
    unittest.main()
