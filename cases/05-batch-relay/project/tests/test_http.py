from __future__ import annotations

import hashlib
import hmac
import json
import tempfile
import threading
import unittest
from http.client import HTTPConnection
from pathlib import Path

from batchrelay.server import RelayHTTPServer
from batchrelay.storage import Store


class HttpTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.store = Store(Path(self.temp.name) / "relay.sqlite3")
        self.server = RelayHTTPServer(("127.0.0.1", 0), "secret", self.store)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.port = self.server.server_address[1]

    def tearDown(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)
        self.temp.cleanup()

    def call(self, method: str, path: str, body: bytes | None = None, signature: str | None = None):
        connection = HTTPConnection("127.0.0.1", self.port, timeout=3)
        headers = {"Content-Type": "application/json"}
        if signature is not None:
            headers["X-Batch-Signature"] = signature
        connection.request(method, path, body=body, headers=headers)
        response = connection.getresponse()
        result = response.read()
        connection.close()
        return response.status, json.loads(result) if result else None

    def test_signature_and_atomic_batch_response(self) -> None:
        batch = {
            "batch_id": "batch",
            "jobs": [{"job_id": "job", "kind": "work", "payload": {"x": 1}, "depends_on": []}],
        }
        body = json.dumps(batch, separators=(",", ":")).encode()
        self.assertEqual(self.call("POST", "/batches", body, "sha256=bad")[0], 401)
        digest = hmac.new(b"secret", body, hashlib.sha256).hexdigest()
        status, result = self.call("POST", "/batches", body, "sha256=" + digest)
        self.assertEqual(status, 202)
        self.assertEqual(result["jobs"][0]["status"], "pending")
        self.assertEqual(self.call("GET", "/batches/batch")[1], result)
