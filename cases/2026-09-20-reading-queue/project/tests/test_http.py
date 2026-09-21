"""End-to-end tests over real HTTP sockets, including a server restart.

These tests start :func:`readqueue.server.serve` in a worker thread bound to an
ephemeral port (``--port 0`` semantics), talk to it with
:mod:`urllib.request`, then close it and start a **new** server over the same
database file.  That mirrors the acceptance check in ``SPEC.md`` and is the only
place where the transport layer (framing, headers, keep-alive) is covered.
"""

from __future__ import annotations

import http.client
import json
import os
import tempfile
import threading
import unittest
from typing import Any
from urllib.error import HTTPError
from urllib.parse import quote
from urllib.request import Request, urlopen

from support import PROJECT_ROOT  # noqa: F401 - ensures ``readqueue`` is importable

from readqueue.api import Api
from readqueue.server import make_server
from readqueue.store import ItemStore


class HTTPTestBase(unittest.TestCase):
    """Runs a real threaded server on a free port with a temp database."""

    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory(prefix="readqueue-http-")
        self.addCleanup(self.directory.cleanup)
        self.db_path = os.path.join(self.directory.name, "nested", "queue.sqlite3")
        self.servers: list[Any] = []
        self.start_server()

    def tearDown(self) -> None:
        for server, store in reversed(self.servers):
            server.shutdown()
            server.server_close()
            store.close()

    # -- server control --------------------------------------------------

    def start_server(self) -> tuple[int, ItemStore]:
        """Open the shared database and start another server on a free port."""
        store = ItemStore(self.db_path)
        server = make_server("127.0.0.1", 0, store, api=Api(store))
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(thread.join, 5)
        self.servers.append((server, store))
        self.port = server.server_address[1]
        self.store = store
        return self.port, store

    def restart_server(self) -> None:
        """Stop the current server, leaving the database file behind."""
        server, store = self.servers.pop()
        server.shutdown()
        server.server_close()
        store.close()
        self.start_server()

    # -- requests --------------------------------------------------------

    def request(self, method: str, path: str, body: Any = None, raw: str | None = None):
        """Perform one request, returning ``(status, decoded body or None)``."""
        data = None
        headers: dict[str, str] = {}
        if raw is not None:
            data = raw.encode("utf-8")
            headers["Content-Type"] = "application/json"
        elif body is not None:
            data = json.dumps(body).encode("utf-8")
            headers["Content-Type"] = "application/json"
        request = Request(f"http://127.0.0.1:{self.port}{path}", data=data, method=method, headers=headers)
        try:
            with urlopen(request) as response:
                return response.status, self._decode(response.read(), response.status)
        except HTTPError as error:
            payload = error.read()
            content_type = error.headers.get("Content-Type", "")
            error.close()
            self.assertTrue(
                content_type.startswith("application/json"),
                f"error responses must be JSON, got {content_type!r}",
            )
            return error.code, self._decode(payload, error.code)

    @staticmethod
    def _decode(payload: bytes, status: int) -> Any:
        if not payload:
            return None
        assert status != 204, "204 responses must not carry a body"
        return json.loads(payload.decode("utf-8"))

    def create(self, title: str, url: str, tags: list[str] | None = None):
        body = {"title": title, "url": url}
        if tags is not None:
            body["tags"] = tags
        return self.request("POST", "/items", body)


class HappyPathTests(HTTPTestBase):
    def test_healthz_over_http(self) -> None:
        status, payload = self.request("GET", "/healthz")
        self.assertEqual((status, payload), (200, {"ok": True}))

    def test_create_filter_read_patch_delete(self) -> None:
        """The full acceptance flow: two items, filter, read, patch, delete."""
        first_status, first = self.create("Design doc", "https://example.test/design", ["Rupi", "docs"])
        second_status, second = self.create("Spec", "https://example.test/spec", ["docs"])
        self.assertEqual(first_status, 201)
        self.assertEqual(
            first, {"id": 1, "title": "Design doc", "url": "https://example.test/design", "status": "queued", "tags": ["docs", "rupi"]}
        )
        self.assertEqual(second_status, 201)

        status, listed = self.request("GET", "/items")
        self.assertEqual(status, 200)
        self.assertEqual([item["id"] for item in listed], [1, 2])

        status, docs = self.request("GET", "/items?tag=docs")
        self.assertEqual([item["id"] for item in docs], [1, 2])
        status, rupi = self.request("GET", "/items?tag=" + quote("RUPI"))
        self.assertEqual([item["id"] for item in rupi], [1])
        status, reading = self.request("GET", "/items?status=reading")
        self.assertEqual(reading, [])

        status, single = self.request("GET", "/items/2")
        self.assertEqual((status, single["title"]), (200, "Spec"))

        status, patched = self.request("PATCH", "/items/2", {"status": "reading", "title": "Spec v2"})
        self.assertEqual(status, 200)
        self.assertEqual(patched["status"], "reading")
        self.assertEqual(patched["title"], "Spec v2")
        status, reading = self.request("GET", "/items?status=reading")
        self.assertEqual([item["id"] for item in reading], [2])

        status, _ = self.request("DELETE", "/items/1")
        self.assertEqual(status, 204)
        status, listed = self.request("GET", "/items")
        self.assertEqual([item["id"] for item in listed], [2])
        self.assertEqual(self.request("GET", "/items/1"), (404, {"error": "no item with id 1"}))

    def test_headers_are_json_and_content_length_is_exact(self) -> None:
        connection = http.client.HTTPConnection("127.0.0.1", self.port)
        self.addCleanup(connection.close)
        connection.request("GET", "/healthz")
        response = connection.getresponse()
        body = response.read()
        self.assertEqual(response.status, 200)
        self.assertEqual(response.getheader("Content-Type"), "application/json")
        self.assertEqual(int(response.getheader("Content-Length")), len(body))

        connection.request("POST", "/items", json.dumps({"title": "A", "url": "https://a.test"}), {"Content-Type": "application/json"})
        created = json.loads(connection.getresponse().read())
        connection.request("DELETE", f"/items/{created['id']}")
        deleted = connection.getresponse()
        self.assertEqual(deleted.status, 204)
        self.assertEqual(deleted.read(), b"")
        self.assertIsNone(deleted.getheader("Content-Length"))
        # Keep-alive survived the 204, so the same connection still works.
        connection.request("GET", "/items")
        self.assertEqual(connection.getresponse().read(), b"[]")

    def test_body_bytes_are_deterministic(self) -> None:
        self.create("B", "https://b.test", ["b", "a"])
        self.create("A", "https://a.test")
        bodies = set()
        for _ in range(3):
            connection = http.client.HTTPConnection("127.0.0.1", self.port)
            self.addCleanup(connection.close)
            connection.request("GET", "/items")
            bodies.add(connection.getresponse().read())
        self.assertEqual(len(bodies), 1)
        payload = bodies.pop().decode("utf-8")
        self.assertEqual(payload, payload.replace('"id"', '"id"'))
        self.assertLess(payload.index('"status"'), payload.index('"tags"'))

    def test_restart_preserves_items_and_updated_state(self) -> None:
        status, created = self.create("Persist me", "https://example.test/persist", ["Rupi"])
        self.assertEqual(status, 201)
        status, updated = self.request("PATCH", f"/items/{created['id']}", {"status": "done"})
        self.assertEqual(status, 200)
        self.assertEqual(updated["status"], "done")

        self.restart_server()

        status, items = self.request("GET", "/items")
        self.assertEqual(status, 200)
        self.assertEqual(items, [updated])


class ErrorHandlingTests(HTTPTestBase):
    def assert_json_error(self, result, status: int) -> dict:
        code, payload = result
        self.assertEqual(code, status, msg=payload)
        self.assertIsInstance(payload, dict)
        self.assertIsInstance(payload.get("error"), str)
        self.assertTrue(payload["error"])
        return payload

    def test_malformed_json_preserves_data(self) -> None:
        status, created = self.create("Keep me", "https://example.test/keep")
        self.assertEqual(status, 201)
        self.assert_json_error(self.request("POST", "/items", raw="{oops"), 400)
        self.assertEqual(self.request("GET", "/items"), (200, [created]))

    def test_unknown_routes(self) -> None:
        self.assert_json_error(self.request("GET", "/missing"), 404)
        self.assert_json_error(self.request("GET", "/"), 404)
        self.assert_json_error(self.request("PUT", "/items/1"), 405)
        self.assert_json_error(self.request("POST", "/items/1", {"title": "x", "url": "u"}), 405)

    def test_validation_errors(self) -> None:
        self.assert_json_error(self.request("POST", "/items", raw="{oops"), 400)
        self.assert_json_error(self.request("POST", "/items", raw="[]"), 400)
        self.assert_json_error(self.request("POST", "/items", {"url": "https://x.test"}), 400)
        self.assert_json_error(self.request("POST", "/items", {"title": "t", "url": "u", "extra": 1}), 400)
        self.assert_json_error(self.request("POST", "/items", {"title": "t", "url": "u", "tags": [""]}), 400)
        self.assert_json_error(self.request("GET", "/items?status=nope"), 400)

    def test_conflicts_and_unknown_ids(self) -> None:
        self.create("A", "https://dup.test")
        self.assert_json_error(self.create("B", "https://dup.test"), 409)
        self.assertEqual(len(self.request("GET", "/items")[1]), 1)
        self.assert_json_error(self.request("GET", "/items/99"), 404)
        self.assert_json_error(self.request("PATCH", "/items/99", {"status": "done"}), 404)
        self.assert_json_error(self.request("DELETE", "/items/99"), 404)
        self.assertEqual(len(self.request("GET", "/items")[1]), 1)
