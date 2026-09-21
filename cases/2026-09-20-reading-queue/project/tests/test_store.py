"""Unit tests for :mod:`readqueue.store` (persistence rules only)."""

from __future__ import annotations

import os
import unittest

from support import StoreTestCase

from readqueue.errors import DuplicateURL, UnknownItem
from readqueue.store import ItemStore


class ItemStoreTests(StoreTestCase):
    def test_creates_database_and_parent_directory(self) -> None:
        """A brand-new path is created together with its parent directory."""
        nested = os.path.join(self._directory.name, "deep", "deeper", "queue.sqlite3")
        store = ItemStore(nested)
        self.addCleanup(store.close)
        self.assertTrue(os.path.isdir(os.path.dirname(nested)))
        self.assertEqual(store.list(), [])

    def test_created_item_starts_queued_with_sequential_ids(self) -> None:
        first = self.store.create("One", "https://example.test/one", ["b", "a"])
        second = self.store.create("Two", "https://example.test/two")
        self.assertEqual(first["id"], 1)
        self.assertEqual(first["status"], "queued")
        self.assertEqual(first["tags"], ["a", "b"])
        self.assertEqual(second["id"], 2)
        self.assertEqual(second["tags"], [])

    def test_list_is_ordered_by_ascending_id(self) -> None:
        for index in range(4):
            self.store.create(f"Item {index}", f"https://example.test/{index}")
        self.assertEqual([item["id"] for item in self.store.list()], [1, 2, 3, 4])

    def test_list_filters_by_status(self) -> None:
        kept = self.store.create("Keep", "https://example.test/keep")
        self.store.create("Drop", "https://example.test/drop")
        self.store.update(kept["id"], {"status": "done"})
        self.assertEqual([i["id"] for i in self.store.list(status="done")], [kept["id"]])
        self.assertEqual([i["id"] for i in self.store.list(status="queued")], [2])
        self.assertEqual(self.store.list(status="reading"), [])

    def test_list_filters_by_tag_case_insensitively(self) -> None:
        item = self.store.create("Tagged", "https://example.test/tagged", ["RuPi"])
        self.assertEqual(item["tags"], ["rupi"])
        self.assertEqual([i["id"] for i in self.store.list(tag="RUP")], [])
        self.assertEqual([i["id"] for i in self.store.list(tag="Rupi")], [item["id"]])
        self.assertEqual([i["id"] for i in self.store.list(tag=" rupi ")], [item["id"]])

    def test_duplicate_url_is_rejected_without_changing_data(self) -> None:
        existing = self.store.create("First", "https://example.test/same")
        with self.assertRaises(DuplicateURL):
            self.store.create("Second", "https://example.test/same")
        self.assertEqual(self.store.count(), 1)
        self.assertEqual(self.store.get(existing["id"])["title"], "First")

    def test_update_changes_only_named_fields(self) -> None:
        item = self.store.create("Old", "https://example.test/keep", ["a"])
        updated = self.store.update(item["id"], {"status": "reading"})
        self.assertEqual(updated["status"], "reading")
        self.assertEqual(updated["title"], "Old")
        self.assertEqual(updated["url"], "https://example.test/keep")
        self.assertEqual(updated["tags"], ["a"])

    def test_update_rejects_url_owned_by_another_item(self) -> None:
        self.store.create("A", "https://example.test/a")
        second = self.store.create("B", "https://example.test/b")
        with self.assertRaises(DuplicateURL):
            self.store.update(second["id"], {"url": "https://example.test/a"})
        self.assertEqual(self.store.get(second["id"])["url"], "https://example.test/b")

    def test_update_allows_same_url_on_same_item(self) -> None:
        item = self.store.create("A", "https://example.test/a")
        updated = self.store.update(item["id"], {"url": "https://example.test/a", "status": "done"})
        self.assertEqual(updated["url"], "https://example.test/a")
        self.assertEqual(updated["status"], "done")

    def test_update_and_delete_raise_unknown_item(self) -> None:
        with self.assertRaises(UnknownItem):
            self.store.update(42, {"status": "done"})
        with self.assertRaises(UnknownItem):
            self.store.delete(42)
        self.assertEqual(self.store.count(), 0)

    def test_update_without_fields_is_rejected(self) -> None:
        item = self.store.create("A", "https://example.test/a")
        with self.assertRaises(ValueError):
            self.store.update(item["id"], {})

    def test_delete_removes_only_the_named_item(self) -> None:
        first = self.store.create("A", "https://example.test/a")
        second = self.store.create("B", "https://example.test/b")
        self.store.delete(first["id"])
        self.assertEqual([i["id"] for i in self.store.list()], [second["id"]])
        with self.assertRaises(UnknownItem):
            self.store.get(first["id"])

    def test_ids_are_never_reused_after_delete(self) -> None:
        item = self.store.create("A", "https://example.test/a")
        self.store.delete(item["id"])
        replacement = self.store.create("A2", "https://example.test/a")
        self.assertEqual(replacement["id"], item["id"] + 1)


class ReopenTests(StoreTestCase):
    def test_second_connection_sees_previously_committed_items(self) -> None:
        """Re-opening the same file (restart) restores items, order and ids."""
        created = self.store.create("Persist", "https://example.test/persist", ["Zeta", "alpha"])
        self.store.update(created["id"], {"status": "reading"})
        self.store.close()

        reopened = ItemStore(self.db_path)
        self.addCleanup(reopened.close)
        items = reopened.list()
        self.assertEqual(len(items), 1)
        self.assertEqual(items[0], created | {"status": "reading", "tags": ["alpha", "zeta"]})


if __name__ == "__main__":  # pragma: no cover
    unittest.main()
