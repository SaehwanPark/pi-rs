from __future__ import annotations

import hashlib
import hmac
import json
from pathlib import Path
import tempfile
import threading
import unittest
from urllib.error import HTTPError
from urllib.request import Request, urlopen

from webhookinbox.server import make_server


class HttpTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        database = Path(self.tempdir.name) / "state.sqlite3"
        self.secret = "test-secret"
        self.server = make_server("127.0.0.1", 0, str(database), self.secret)
        self.port = self.server.server_port
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def tearDown(self) -> None:
        self.server.shutdown()
        self.thread.join(timeout=3)
        self.server.server_close()
        self.tempdir.cleanup()

    def _request(
        self,
        method: str,
        path: str,
        body: bytes | None = None,
        signed: bool = True,
    ) -> tuple[int, dict[str, object]]:
        headers = {"Content-Type": "application/json"}
        if signed and body is not None:
            digest = hmac.new(
                self.secret.encode(), body, hashlib.sha256
            ).hexdigest()
            headers["X-Webhook-Signature"] = f"sha256={digest}"
        request = Request(
            f"http://127.0.0.1:{self.port}{path}",
            data=body,
            headers=headers,
            method=method,
        )
        try:
            with urlopen(request, timeout=3) as response:
                return int(response.status), json.loads(response.read())
        except HTTPError as error:
            with error:
                return int(error.code), json.loads(error.read())

    @staticmethod
    def _body(value: object) -> bytes:
        return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()

    def test_health_and_signed_idempotent_admission(self) -> None:
        status, health = self._request("GET", "/healthz", signed=False)
        self.assertEqual((status, health), (200, {"ok": True}))
        value = {"delivery_id": "del-1", "event_type": "release", "payload": {}}
        body = self._body(value)
        status, created = self._request("POST", "/deliveries", body)
        self.assertEqual(status, 202)
        self.assertEqual(created["status"], "pending")
        status, duplicate = self._request("POST", "/deliveries", body)
        self.assertEqual(status, 200)
        self.assertEqual(duplicate, created)
        changed = self._body({**value, "payload": {"changed": True}})
        status, conflict = self._request("POST", "/deliveries", changed)
        self.assertEqual(status, 409)
        self.assertIn("error", conflict)

    def test_invalid_signature_and_method_do_not_write(self) -> None:
        value = {"delivery_id": "del-1", "event_type": "release", "payload": {}}
        body = self._body(value)
        status, error = self._request("POST", "/deliveries", body, signed=False)
        self.assertEqual(status, 401)
        self.assertIn("error", error)
        status, error = self._request("PUT", "/deliveries", body, signed=False)
        self.assertEqual(status, 405)
        self.assertIn("error", error)
        status, error = self._request("GET", "/deliveries/del-1", signed=False)
        self.assertEqual(status, 404)
        self.assertIn("error", error)


if __name__ == "__main__":
    unittest.main()
