"""CLI behaviour: normal flows, help, invalid input, and unchanged-state safety.

Every test runs the real argument parser and the real state file, so a passing
test means two separate ``python -m tasklog`` invocations would agree.
"""

from __future__ import annotations

import json
import os
import unittest
from unittest import mock

from tasklog.cli import EXIT_ERROR, EXIT_OK, EXIT_USAGE, build_parser

from support import TempDirCase


class CliCase(TempDirCase):
    """Small helpers specific to asserting on CLI output."""

    def expect_error(self, *argv: str) -> tuple[str, str]:
        """Run ``argv`` expecting failure and return ``(stdout, stderr)``.

        Also asserts the shared contract for every rejected command: exit 2, a
        message on stderr, nothing on stdout.
        """
        code, out, err = self.run_cli(*argv)
        self.assertEqual(code, EXIT_ERROR, msg=f"argv={argv!r}")
        self.assertEqual(out, "", msg="failed commands must print nothing")
        self.assertTrue(err.strip(), msg="failed commands must explain itself")
        return out, err


class HelpTests(CliCase):
    def test_help_documents_commands_and_persistence(self):
        code, out, err = self.run_cli("--help")
        self.assertEqual(code, 0)
        self.assertEqual(err, "")
        for expected in ("add", "list", "done", "remove", "--all", "--state"):
            self.assertIn(expected, out)
        # Persistence must be documented, not just the commands.
        self.assertIn(".tasklog.json", out)
        self.assertIn("TASKLOG_PATH", out)
        self.assertIn("atomically", out)

    def test_bare_invocation_shows_help_with_nonzero_status(self):
        code, out, err = self.run_cli()
        self.assertEqual(code, EXIT_USAGE)
        self.assertIn("usage:", out)

    def test_version(self):
        code, out, _ = self.run_cli("--version")
        self.assertEqual(code, 0)
        self.assertIn("tasklog", out)

    def test_parser_exposes_all_subcommands(self):
        parser = build_parser()
        actions = [
            action
            for action in parser._actions  # noqa: SLF001 - inspecting our parser
            if getattr(action, "choices", None)
            and callable(getattr(action, "metavar", None)) is False
        ]
        self.assertTrue(actions)
        choices = actions[0].choices
        self.assertEqual(sorted(choices), ["add", "done", "list", "remove"])


class FreshDirectoryTests(CliCase):
    def test_list_in_fresh_directory_succeeds_without_state_file(self):
        out = self.must_run("list")
        self.assertIn("No open tasks", out)
        self.assertFalse(self.state.exists())

    def test_list_all_in_fresh_directory_succeeds(self):
        out = self.must_run("list", "--all")
        self.assertIn("No tasks", out)
        self.assertFalse(self.state.exists())


class AddTests(CliCase):
    def test_add_prints_task_and_persists_it(self):
        out = self.must_run("add", "write", "tests")
        self.assertEqual(out.strip(), "Added task 1: write tests")
        document = self.state_document()
        self.assertEqual(document["tasks"][0]["text"], "write tests")
        self.assertFalse(document["tasks"][0]["done"])
        self.assertEqual(document["next_id"], 2)

    def test_state_file_appears_only_on_first_successful_write(self):
        self.assertFalse(self.state.exists())
        self.must_run("list")
        self.assertFalse(self.state.exists())
        self.must_run("add", "first")
        self.assertTrue(self.state.exists())

    def test_ids_are_stable_positive_integers(self):
        ids = []
        for index in range(1, 4):
            out = self.must_run("add", f"task {index}")
            ids.append(int(out.split()[2].rstrip(":")))
        self.assertEqual(ids, [1, 2, 3])

    def test_add_collapses_surrounding_whitespace(self):
        out = self.must_run("add", "  ship   it ")
        self.assertEqual(out.strip(), "Added task 1: ship it")

    def test_add_rejects_blank_text_without_creating_file(self):
        self.expect_error("add", "   ")
        self.assertFalse(self.state.exists())

    def test_add_after_removal_never_reuses_id(self):
        self.must_run("add", "one")
        self.must_run("add", "two")
        self.must_run("remove", "2")
        out = self.must_run("add", "three")
        self.assertEqual(out.strip(), "Added task 3: three")
        document = self.state_document()
        self.assertEqual([t["id"] for t in document["tasks"]], [1, 3])
        self.assertEqual(document["next_id"], 4)


class ListTests(CliCase):
    def test_list_shows_open_tasks_in_ascending_order_with_summary(self):
        self.must_run("add", "alpha")
        self.must_run("add", "beta")
        self.must_run("done", "1")
        self.must_run("add", "gamma")
        out = self.must_run("list")
        lines = out.splitlines()
        self.assertEqual(lines[-1], "2 open")
        self.assertEqual(
            lines[:2], ["   2 [ ] beta", "   3 [ ] gamma"]
        )
        self.assertNotIn("1 [x] alpha", out)

    def test_list_all_includes_completed_and_counts_both(self):
        self.must_run("add", "alpha")
        self.must_run("add", "beta")
        self.must_run("done", "1")
        out = self.must_run("list", "--all")
        self.assertEqual(
            out.splitlines(),
            ["   1 [x] alpha", "   2 [ ] beta", "1 open, 1 done"],
        )

    def test_short_all_flag_matches_long_flag(self):
        self.must_run("add", "alpha")
        self.must_run("done", "1")
        self.assertEqual(
            self.must_run("list", "-a").splitlines(),
            self.must_run("list", "--all").splitlines(),
        )


class DoneTests(CliCase):
    def test_done_marks_task_and_persists_across_invocations(self):
        self.must_run("add", "alpha")
        out = self.must_run("done", "1")
        self.assertEqual(out.strip(), "Completed task 1: alpha")
        self.assertTrue(self.state_document()["tasks"][0]["done"])

    def test_done_is_idempotent_and_does_not_rewrite_state(self):
        self.must_run("add", "alpha")
        self.must_run("done", "1")
        before = self.state_bytes()
        out = self.must_run("done", "1")
        self.assertIn("already done", out)
        self.assertEqual(self.state_bytes(), before)

    def test_done_accepts_surrounding_whitespace(self):
        self.must_run("add", "alpha")
        self.must_run("done", " 1 ")
        self.assertTrue(self.state_document()["tasks"][0]["done"])

    def test_done_reports_remaining_state_clearly(self):
        self.must_run("add", "alpha")
        self.must_run("add", "beta")
        self.must_run("done", "1")
        self.must_run("done", "1")
        out = self.must_run("list", "--all")
        self.assertEqual(out.splitlines()[-1], "1 open, 1 done")


class RemoveTests(CliCase):
    def test_remove_deletes_and_keeps_next_id(self):
        self.must_run("add", "alpha")
        self.must_run("add", "beta")
        out = self.must_run("remove", "1")
        self.assertIn("Removed task 1: alpha", out)
        document = self.state_document()
        self.assertEqual([t["id"] for t in document["tasks"]], [2])
        self.assertEqual(document["next_id"], 3)

    def test_remove_completed_task_works(self):
        self.must_run("add", "alpha")
        self.must_run("done", "1")
        self.must_run("remove", "1")
        self.assertEqual(self.state_document()["tasks"], [])

    def test_completed_tasks_survive_until_removed(self):
        self.must_run("add", "alpha")
        self.must_run("done", "1")
        self.must_run("add", "beta")
        self.must_run("done", "2")
        document = self.state_document()
        self.assertEqual(len(document["tasks"]), 2)
        self.assertTrue(all(t["done"] for t in document["tasks"]))


class StatePathTests(CliCase):
    def test_state_option_redirects_the_file(self):
        custom = self.base / "custom" / "ledger.json"
        custom.parent.mkdir()
        self.must_run("--help")  # sanity: global flag still works
        out, _ = self.run_cli_with_state(custom, "add", "via option")
        self.assertIn("Added task 1", out)
        self.assertTrue(custom.exists())
        self.assertFalse(self.state.exists())

    def test_environment_variable_redirects_the_file(self):
        custom = self.base / "env-ledger.json"
        with mock.patch.dict(os.environ, {"TASKLOG_PATH": str(custom)}):
            out = self.must_run("add", "via env")
        self.assertIn("Added task 1: via env", out)
        self.assertTrue(custom.exists())
        self.assertFalse(self.state.exists())
        with mock.patch.dict(os.environ, {"TASKLOG_PATH": str(custom)}):
            listing = self.must_run("list")
        self.assertIn("via env", listing)

    def run_cli_with_state(self, path, *argv):
        code, out, err = self.run_cli("--state", str(path), *argv)
        self.assertEqual(code, EXIT_OK, msg=f"stderr={err!r}")
        return out, err


class InvalidInputTests(CliCase):
    """Malformed commands must fail loudly and leave the file alone."""

    def setUp(self):
        super().setUp()
        self.must_run("add", "keep me")
        self.must_run("add", "keep me too")
        self.before = self.state_bytes()

    def test_malformed_ids_are_rejected(self):
        for bad in ("abc", "0", "-1", "1.5", "", "  ", "1x", "+", "١", "01x", "None"):
            with self.subTest(bad=bad):
                _, err = self.expect_error("done", bad)
                self.assertIn("invalid id", err)
                self.assertIn("positive", err)
        for bad in ("abc", "0", "-7", "two"):
            with self.subTest(bad=bad):
                _, err = self.expect_error("remove", bad)
                self.assertIn("invalid id", err)

    def test_missing_ids_are_rejected(self):
        for command in ("done", "remove"):
            with self.subTest(command=command):
                code, _out, err = self.run_cli(command)
                self.assertEqual(code, EXIT_USAGE)
                self.assertIn("required", err)

    def test_missing_text_is_rejected(self):
        code, _out, err = self.run_cli("add")
        self.assertEqual(code, EXIT_USAGE)
        self.assertIn("required", err)

    def test_unknown_command_is_rejected(self):
        code, _out, err = self.run_cli("bogus")
        self.assertEqual(code, EXIT_USAGE)
        self.assertIn("invalid choice", err)
        self.assertIn("add", err)

    def test_unknown_subcommand_option_is_rejected(self):
        code, _out, err = self.run_cli("list", "--nope")
        self.assertEqual(code, EXIT_USAGE)
        self.assertIn("unrecognized", err)

    def test_unknown_id_is_rejected_with_corrective_hint(self):
        _out, err = self.expect_error("done", "999")
        self.assertIn("no task with id 999", err)
        self.assertIn("list --all", err)
        _out, err = self.expect_error("remove", "999")
        self.assertIn("no task with id 999", err)

    def test_removed_id_is_no_longer_addressable(self):
        self.must_run("remove", "2")
        _out, err = self.expect_error("done", "2")
        self.assertIn("no task with id 2", err)

    def test_state_file_unchanged_by_every_rejected_command(self):
        # The central safety claim of this test class: the loop above must not
        # have altered the ledger, byte for byte, nor left temp files behind.
        self.expect_error("done", "0")
        self.expect_error("done", "999")
        self.expect_error("remove", "nope")
        self.expect_error("done", "1.5")
        self.assertEqual(self.state_bytes(), self.before)
        self.assertEqual(self.temp_files(), [])


class CorruptStateTests(CliCase):
    """An unreadable ledger must be reported, never silently replaced."""

    def test_each_command_refuses_bad_state_and_preserves_bytes(self):
        broken = "{ this is not json"
        self.write_state(broken)
        for argv in (
            ("list",),
            ("list", "--all"),
            ("add", "new task"),
            ("done", "1"),
            ("remove", "1"),
        ):
            with self.subTest(argv=argv):
                _out, err = self.expect_error(*argv)
                self.assertIn("not valid JSON", err)
                self.assertEqual(self.state.read_text(encoding="utf-8"), broken)

    def test_semantically_invalid_document_is_refused(self):
        self.write_state(
            {"version": 1, "next_id": 1, "tasks": [{"id": 1, "text": "x", "done": "yes"}]}
        )
        before = self.state_bytes()
        _out, err = self.expect_error("add", "more")
        self.assertIn("true or false", err)
        self.assertEqual(self.state_bytes(), before)

    def test_unsupported_version_is_refused(self):
        self.write_state({"version": 2, "next_id": 1, "tasks": []})
        _out, err = self.expect_error("list")
        self.assertIn("unsupported version", err)

    def test_hand_written_valid_document_is_accepted(self):
        # Humans edit this file; a valid hand-written ledger must just work.
        self.write_state(
            json.dumps({"version": 1, "tasks": [{"id": 7, "text": "by hand"}]})
        )
        out = self.must_run("list")
        self.assertIn("7 [ ] by hand", out)
        out = self.must_run("add", "next one")
        self.assertIn("Added task 8: next one", out)

    def test_valid_tasks_survive_a_rejected_command(self):
        self.write_state({"version": 1, "next_id": 3, "tasks": [
            {"id": 1, "text": "valid one", "done": False},
            {"id": 2, "text": "valid two", "done": False},
        ]})
        self.expect_error("done", "42")
        document = self.state_document()
        self.assertEqual([t["text"] for t in document["tasks"]], ["valid one", "valid two"])


class EndToEndSmokeTests(CliCase):
    """The exact sequence from the specification, one invocation at a time."""

    def test_spec_smoke_sequence(self):
        self.assertIn("Added task 1", self.must_run("add", "write", "the", "docs"))
        self.assertIn("Added task 2", self.must_run("add", "run", "the", "tests"))

        listing = self.must_run("list")
        self.assertEqual(
            listing.splitlines(),
            ["   1 [ ] write the docs", "   2 [ ] run the tests", "2 open"],
        )

        self.assertIn("Completed task 1", self.must_run("done", "1"))
        listing = self.must_run("list", "--all")
        self.assertEqual(
            listing.splitlines(),
            ["   1 [x] write the docs", "   2 [ ] run the tests", "1 open, 1 done"],
        )

        self.assertIn("Removed task 2", self.must_run("remove", "2"))

        document = self.state_document()
        self.assertEqual(
            document,
            {
                "next_id": 3,
                "tasks": [{"done": True, "id": 1, "text": "write the docs"}],
                "version": 1,
            },
        )
        # Only the intended file exists in the working directory.
        self.assertEqual(sorted(p.name for p in self.base.iterdir()), [".tasklog.json"])
        self.assertEqual(EXIT_OK, self.run_cli("list", "--all")[0])


if __name__ == "__main__":  # pragma: no cover
    unittest.main()
