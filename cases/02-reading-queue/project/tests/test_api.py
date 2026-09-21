"""Unit tests for :mod:`readqueue.api` (routes, status codes, determinism).

The dispatch layer is exercised without sockets: ``api.handle`` returns a
:class:`~readqueue.api.Response`, so the exact JSON body, status, and headers
of every documented rule are asserted in-process.
"""

from __future__ import annotations

import json
import unittest
from urllib.parse import parse_qs, urlparse

from support import StoreTestCase

from readqueue.api import JSON_CONTENT_TYPE, dumps


class ResponseMixin:
    """Shorthand for calling the API the way the HTTP handler does."""

    def call(self, method: str, path: str, body=None, raw_body: str | None = None):
        query = parse_qs(urlparse(path).query, keep_blank_values=True)
        clean = urlparse(path).path
        payload = raw_body if raw_body is not None else (
            b"" if body is None else json.dumps(body).encode("utf-8")
        )
        return self.api.handle(method, clean, query, payload)


class HealthTests(StoreTestCase, ResponseMixin):
    def test_healthz_without_items(self) -> None:
        response = self.call("GET", "/healthz")
        self.assertEqual(response.status, 200)
        self.assertEqual(self.json_of(response), {"ok": True})
        self.assertEqual(response.content_type(), JSON_CONTENT_TYPE)

    def test_healthz_rejects_other_methods(self) -> None:
        self.assertError(self.call("POST", "/healthz"), 405, "method not allowed")


class CollectionTests(StoreTestCase, ResponseMixin):
    def test_empty_list(self) -> None:
        response = self.call("GET", "/items")
        self.assertEqual(response.status, 200)
        self.assertEqual(self.json_of(response), [])

    def test_list_order_is_ascending_id(self) -> None:
        for index in range(3):
            self.call("POST", "/items", {"title": f"T{index}", "url": f"https://example.test/{index}"})
        listed = [item["id"] for item in self.json_of(self.call("GET", "/items"))]
        self.assertEqual(listed, [1, 2, 3])

    def test_created_item_shape(self) -> None:
        response = self.call(
            "POST",
            "/items",
            {"title": "Read the design", "url": "https://example.test/design", "tags": ["rupi", "Docs", "rupi"]},
        )
        self.assertEqual(response.status, 201)
        self.assertEqual(
            self.json_of(response),
            {
                "id": 1,
                "title": "Read the design",
                "url": "https://example.test/design",
                "status": "queued",
                "tags": ["docs", "rupi"],
            },
        )

    def test_filters_by_status_and_tag(self) -> None:
        first = self.json_of(self.call("POST", "/items", {"title": "A", "url": "https://a.test", "tags": ["Papers"]}))
        second = self.json_of(self.call("POST", "/items", {"title": "B", "url": "https://b.test", "tags": ["docs"]}))
        self.call("PATCH", f"/items/{second['id']}", {"status": "reading"})

        self.assertEqual([i["id"] for i in self.json_of(self.call("GET", "/items?status=queued"))], [first["id"]])
        self.assertEqual([i["id"] for i in self.json_of(self.call("GET", "/items?status=reading"))], [second["id"]])
        self.assertEqual(self.json_of(self.call("GET", "/items?status=done")), [])
        self.assertEqual([i["id"] for i in self.json_of(self.call("GET", "/items?tag=PAPERS"))], [first["id"]])
        self.assertEqual(self.json_of(self.call("GET", "/items?tag=nope")), [])
        self.assertEqual(
            [i["id"] for i in self.json_of(self.call("GET", "/items?status=queued&tag=papers"))],
            [first["id"]],
        )

    def test_invalid_filter_returns_400(self) -> None:
        self.assertError(self.call("GET", "/items?status=started"), 400, "status must be one of")
        self.assertError(self.call("GET", "/items?status="), 400)

    def test_duplicate_url_returns_conflict(self) -> None:
        self.call("POST", "/items", {"title": "A", "url": "https://dup.test"})
        response = self.call("POST", "/items", {"title": "B", "url": "https://dup.test"})
        self.assertError(response, 409, "already")
        self.assertEqual(len(self.json_of(self.call("GET", "/items"))), 1)

    def test_post_validation_matrix(self) -> None:
        invalid = [
            (None, "not JSON object"),
            ([], "array is not an object"),
            ({"title": "T"}, "missing url"),
            ({"url": "u"}, "missing title"),
            ({"title": "", "url": "u"}, "empty title"),
            ({"title": "T", "url": ""}, "empty url"),
            ({"title": 5, "url": "u"}, "title type"),
            ({"title": "T", "url": "u", "tags": "a"}, "tags not list"),
            ({"title": "T", "url": "u", "tags": [1]}, "tag not string"),
            ({"title": "T", "url": "u", "tags": [" "]}, "empty tag"),
            ({"title": "T", "url": "u", "status": "queued"}, "status not accepted"),
            ({"title": "T", "url": "u", "nope": 1}, "unknown field"),
        ]
        for body, why in invalid:
            response = self.call("POST", "/items", body)
            self.assertError(response, 400)
            self.assertEqual(self.store.count(), 0, msg=why)

    def test_malformed_json_is_rejected_without_writing(self) -> None:
        response = self.call("POST", "/items", raw_body="{not json")
        self.assertError(response, 400, "malformed JSON")
        self.assertEqual(self.store.count(), 0)

    def test_empty_body_is_rejected(self) -> None:
        for raw in ("", "   ", b""):
            response = self.api.handle("POST", "/items", {}, raw)
            self.assertError(response, 400)

    def test_method_not_allowed_on_collection(self) -> None:
        response = self.call("DELETE", "/items")
        self.assertError(response, 405)
        self.assertIn("GET", response.headers["Allow"])


class ItemTests(StoreTestCase, ResponseMixin):
    def setUp(self) -> None:
        super().setUp()
        self.item = self.json_of(self.call("POST", "/items", {"title": "A", "url": "https://a.test", "tags": ["x"]}))

    def test_get_existing_and_unknown(self) -> None:
        found = self.call("GET", f"/items/{self.item['id']}")
        self.assertEqual(found.status, 200)
        self.assertEqual(self.json_of(found), self.item)
        self.assertError(self.call("GET", "/items/999"), 404, "no item with id 999")
        self.assertEqual(self.store.count(), 1)

    def test_non_numeric_id_is_not_found(self) -> None:
        for path in ("/items/abc", "/items/0", "/items/-1", "/items/1.5"):
            self.assertError(self.call("GET", path), 404)

    def test_patch_fields_and_status(self) -> None:
        response = self.call(
            "PATCH",
            f"/items/{self.item['id']}",
            {"title": "A2", "url": "https://a2.test", "tags": ["Z", "a"], "status": "done"},
        )
        self.assertEqual(response.status, 200)
        self.assertEqual(
            self.json_of(response),
            {"id": 1, "title": "A2", "url": "https://a2.test", "status": "done", "tags": ["a", "z"]},
        )

    def test_patch_single_field_keeps_others(self) -> None:
        updated = self.json_of(self.call("PATCH", f"/items/{self.item['id']}", {"status": "reading"}))
        self.assertEqual(updated["status"], "reading")
        self.assertEqual(updated["title"], "A")
        self.assertEqual(updated["tags"], ["x"])

    def test_patch_rejects_bad_bodies_and_keeps_item(self) -> None:
        for body, why in [
            ({}, "empty object"),
            ({"status": "sleeping"}, "unknown status"),
            ({"title": ""}, "empty title"),
            ({"tags": [1]}, "non-string tag"),
            ({"nope": 1}, "unknown field"),
            (None, "array body"),
        ]:
            response = self.call("PATCH", f"/items/{self.item['id']}", body)
            self.assertError(response, 400)
            self.assertEqual(self.json_of(self.call("GET", f"/items/{self.item['id']}")), self.item, msg=why)

    def test_patch_malformed_json(self) -> None:
        self.assertError(self.call("PATCH", "/items/1", raw_body="{"), 400, "malformed JSON")

    def test_patch_url_collision_keeps_both_items(self) -> None:
        other = self.json_of(self.call("POST", "/items", {"title": "B", "url": "https://b.test"}))
        response = self.call("PATCH", f"/items/{other['id']}", {"url": self.item["url"]})
        self.assertError(response, 409)
        self.assertEqual(self.json_of(self.call("GET", f"/items/{other['id']}")), other)

    def test_patch_unknown_id_returns_404(self) -> None:
        self.assertError(self.call("PATCH", "/items/77", {"status": "done"}), 404)

    def test_delete_returns_204_without_body(self) -> None:
        response = self.call("DELETE", f"/items/{self.item['id']}")
        self.assertEqual(response.status, 204)
        self.assertFalse(response.has_body)
        self.assertEqual(response.body(), b"")
        self.assertEqual(self.json_of(self.call("GET", "/items")), [])

    def test_delete_unknown_id_leaves_items(self) -> None:
        self.assertError(self.call("DELETE", "/items/55"), 404)
        self.assertEqual(len(self.json_of(self.call("GET", "/items"))), 1)

    def test_method_not_allowed_on_item(self) -> None:
        response = self.call("POST", f"/items/{self.item['id']}", {})
        self.assertError(response, 405)
        self.assertIn("PATCH", response.headers["Allow"])
        self.assertEqual(self.store.count(), 1)


class RoutingTests(StoreTestCase, ResponseMixin):
    def test_unknown_paths_are_404_json(self) -> None:
        for path in ("/", "/nope", "/item", "/items/1/2", "/health"):
            response = self.call("GET", path)
            self.assertError(response, 404, "not found")

    def test_trailing_slash_is_tolerated(self) -> None:
        self.assertEqual(self.call("GET", "/healthz/").status, 200)
        self.assertEqual(self.call("GET", "/items/").status, 200)

    def test_lowercase_method_is_normalised(self) -> None:
        self.assertEqual(self.api.handle("get", "/healthz", {}, b"").status, 200)


class DeterminismTests(StoreTestCase, ResponseMixin):
    def test_object_keys_are_sorted_in_every_body(self) -> None:
        created = self.call(
            "POST", "/items", {"title": "A", "url": "https://a.test", "tags": ["b", "a"]}
        )
        self.assertEqual(
            created.body().decode("utf-8"),
            '{"id":1,"status":"queued","tags":["a","b"],"title":"A","url":"https://a.test"}',
        )

    def test_repeated_requests_render_identical_bytes(self) -> None:
        self.call("POST", "/items", {"title": "A", "url": "https://a.test", "tags": ["z", "y"]})
        self.call("POST", "/items", {"title": "B", "url": "https://b.test", "tags": ["m"]})
        first = [self.call("GET", "/items").body() for _ in range(3)]
        self.assertEqual(set(first), {first[0]})

    def test_dumps_sorts_nested_keys(self) -> None:
        self.assertEqual(dumps({"b": {"d": 1, "c": 2}, "a": [1, {"z": 0, "y": 1}]}),
                         '{"a":[1,{"y":1,"z":0}],"b":{"c":2,"d":1}}')


if __name__ == "__main__":  # pragma: no cover
    unittest.main()
