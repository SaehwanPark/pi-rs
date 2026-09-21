"""Pure-model behaviour: ids, state transitions, validation, document shape."""

from __future__ import annotations

import unittest

from tasklog.model import (
    STATE_VERSION,
    CorruptState,
    InvalidTaskText,
    Ledger,
    LedgerError,
    Task,
    TaskNotFound,
    parse_id,
)


class ParseIdTests(unittest.TestCase):
    def test_accepts_ints_and_int_like_strings(self):
        self.assertEqual(parse_id(3), 3)
        self.assertEqual(parse_id("3"), 3)
        self.assertEqual(parse_id(" 7 "), 7)
        self.assertEqual(parse_id(10**12), 10**12)

    def test_rejects_non_positive_or_non_numeric(self):
        for value in (0, -1, "0", "-4", "1.5", "abc", "", "  ", "+", "١", None, True, False, 1.0, [1]):
            with self.subTest(value=value):
                with self.assertRaises(LedgerError):
                    parse_id(value)

    def test_rejected_id_explains_corrective_action(self):
        with self.assertRaises(LedgerError) as caught:
            parse_id("twelve")
        message = str(caught.exception)
        self.assertIn("twelve", message)
        self.assertIn("positive", message)


class TaskTests(unittest.TestCase):
    def test_task_is_frozen(self):
        task = Task(id=1, text="write tests")
        with self.assertRaises(Exception):
            task.done = True  # type: ignore[misc]

    def test_format_and_status(self):
        # The id column is right-aligned to four characters for alignment.
        self.assertEqual(Task(1, "a").format(), "   1 [ ] a")
        self.assertEqual(Task(12345, "a", done=True).format(), "12345 [x] a")
        self.assertEqual(Task(1, "a", done=True).status, "done")
        self.assertEqual(Task(1, "a").status, "open")


class LedgerAddTests(unittest.TestCase):
    def test_add_assigns_increasing_ids_and_keeps_ledger_pure(self):
        ledger = Ledger.empty()
        ledger, first = ledger.add("first")
        ledger, second = ledger.add("second")
        self.assertEqual((first.id, second.id), (1, 2))
        self.assertTrue(first.done is False)
        # The original empty ledger is untouched: operations return new values.
        self.assertEqual(Ledger.empty().tasks, ())
        self.assertEqual(ledger.next_id, 3)

    def test_add_collapses_whitespace_and_rejects_blank_or_non_text(self):
        ledger, task = Ledger.empty().add("  ship   the  release \n")
        self.assertEqual(task.text, "ship the release")
        for bad in ("", "   ", "\t\n"):
            with self.subTest(bad=bad):
                with self.assertRaises(InvalidTaskText):
                    Ledger.empty().add(bad)
        for bad in (None, 3, ["a"]):
            with self.subTest(bad=bad):
                with self.assertRaises(InvalidTaskText):
                    Ledger.empty().add(bad)


class LedgerDoneTests(unittest.TestCase):
    def setUp(self):
        ledger, _ = Ledger.empty().add("one")
        self.ledger, _ = ledger.add("two")

    def test_done_marks_only_the_target_task(self):
        updated, task, changed = self.ledger.mark_done(1)
        self.assertTrue(changed)
        self.assertEqual(task.text, "one")
        self.assertTrue(updated.get(1).done)
        self.assertFalse(updated.get(2).done)

    def test_done_is_idempotent_and_returns_same_ledger(self):
        done, _, _ = self.ledger.mark_done(1)
        again, task, changed = done.mark_done(1)
        self.assertFalse(changed)
        self.assertIs(again, done)
        self.assertEqual(task.text, "one")

    def test_done_unknown_id_raises_without_touching_ledger(self):
        with self.assertRaises(TaskNotFound):
            self.ledger.mark_done(99)
        self.assertEqual(self.ledger.counts(), (2, 0))

    def test_done_accepts_string_id(self):
        updated, task, changed = self.ledger.mark_done("2")
        self.assertTrue(changed)
        self.assertEqual(task.id, 2)


class LedgerRemoveTests(unittest.TestCase):
    def test_remove_deletes_task_and_never_reuses_id(self):
        ledger, _ = Ledger.empty().add("one")
        ledger, _ = ledger.add("two")
        ledger, removed = ledger.remove(1)
        self.assertEqual(removed.text, "one")
        self.assertEqual([t.id for t in ledger.tasks], [2])
        self.assertEqual(ledger.next_id, 3)
        ledger, third = ledger.add("three")
        self.assertEqual(third.id, 3)

    def test_remove_unknown_id_raises(self):
        with self.assertRaises(TaskNotFound):
            Ledger.empty().remove(1)

    def test_remove_keeps_completed_tasks_until_removed(self):
        ledger, _ = Ledger.empty().add("one")
        ledger, _ = ledger.add("two")
        ledger, _, _ = ledger.mark_done(1)
        ledger, _ = ledger.remove(2)
        self.assertEqual([t.id for t in ledger.tasks], [1])
        self.assertTrue(ledger.get(1).done)


class LedgerQueryTests(unittest.TestCase):
    def _ledger(self):
        ledger = Ledger.empty()
        for index in range(1, 4):
            ledger, _ = ledger.add(f"task {index}")
        ledger, _, _ = ledger.mark_done(2)
        return ledger

    def test_open_tasks_sorted_ascending(self):
        ledger = self._ledger()
        self.assertEqual([t.id for t in ledger.open_tasks()], [1, 3])
        self.assertEqual([t.id for t in ledger.sorted_tasks()], [1, 2, 3])
        self.assertEqual(ledger.counts(), (2, 1))
        self.assertEqual(ledger.summary(), "2 open, 1 done")

    def test_find_and_get(self):
        ledger = self._ledger()
        self.assertEqual(ledger.find(9).id if ledger.find(9) else None, None)
        self.assertEqual(ledger.get(2).text, "task 2")
        with self.assertRaises(TaskNotFound):
            ledger.get(9)

    def test_summary_of_empty_ledger(self):
        self.assertEqual(Ledger.empty().summary(), "empty")
        self.assertEqual(Ledger.empty().counts(), (0, 0))

    def test_duplicate_ids_rejected_on_construction(self):
        with self.assertRaises(CorruptState):
            Ledger(next_id=1, tasks=(Task(1, "a"), Task(1, "b")))

    def test_next_id_must_be_above_existing_ids_on_construction(self):
        with self.assertRaises(CorruptState):
            Ledger(next_id=2, tasks=(Task(3, "a"),))


class DocumentTests(unittest.TestCase):
    def _sample(self):
        ledger, _ = Ledger.empty().add("one")
        ledger, _ = ledger.add("two")
        ledger, _, _ = ledger.mark_done(1)
        return ledger

    def test_document_is_versioned_and_sorted(self):
        document = self._sample().as_document()
        self.assertEqual(document["version"], STATE_VERSION)
        self.assertEqual(document["next_id"], 3)
        self.assertEqual([t["id"] for t in document["tasks"]], [1, 2])
        self.assertEqual(
            set(document["tasks"][0]), {"id", "text", "done"}
        )

    def test_round_trip_preserves_ledger(self):
        ledger = self._sample()
        self.assertEqual(Ledger.from_document(ledger.as_document()), ledger)

    def test_from_document_rejects_bad_shapes(self):
        good = self._sample().as_document()
        cases = {
            "not an object": [],
            "unknown top-level key": {**good, "extra": 1},
            "bad version": {**good, "version": 99},
            "bool version": {**good, "version": True},
            "tasks not a list": {**good, "tasks": {}},
            "task not an object": {**good, "tasks": ["one"]},
            "task unknown key": {
                **good,
                "tasks": [{**good["tasks"][0], "tag": "x"}, good["tasks"][1]],
            },
            "task id zero": {
                **good,
                "tasks": [{**good["tasks"][0], "id": 0}, good["tasks"][1]],
            },
            "task non-bool flag": {
                **good,
                "tasks": [{**good["tasks"][0], "done": "yes"}, good["tasks"][1]],
            },
            "task blank text": {
                **good,
                "tasks": [{**good["tasks"][0], "text": "  "}, good["tasks"][1]],
            },
            "duplicate ids": {**good, "tasks": [good["tasks"][0], dict(good["tasks"][0])]},
            "next_id not above ids": {**good, "next_id": 2},
            "next_id not numeric": {**good, "next_id": "x"},
            "next_id negative": {**good, "next_id": -2},
        }
        for label, document in cases.items():
            with self.subTest(case=label):
                with self.assertRaises(CorruptState):
                    Ledger.from_document(document)

    def test_from_document_tolerates_hand_written_file(self):
        ledger = Ledger.from_document(
            {"version": STATE_VERSION, "tasks": [{"id": 4, "text": "hand", "done": False}]}
        )
        self.assertEqual(ledger.next_id, 5)
        self.assertEqual(ledger.get(4).text, "hand")
        ledger2 = Ledger.from_document({"version": STATE_VERSION, "tasks": []})
        self.assertEqual(ledger2, Ledger.empty())


if __name__ == "__main__":  # pragma: no cover
    unittest.main()
