"""Independent fresh-process acceptance oracle for the Read Queue service."""

from __future__ import annotations

import json
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen


PROJECT = Path(__file__).resolve().parents[1] / "project"


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


class ReadQueueAcceptance(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.db = self.root / "queue.sqlite"
        self.port = free_port()
        self.base = f"http://127.0.0.1:{self.port}"
        self.server = subprocess.Popen(
            [
                sys.executable,
                "-m",
                "readqueue",
                "serve",
                "--db",
                str(self.db),
                "--host",
                "127.0.0.1",
                "--port",
                str(self.port),
            ],
            cwd=PROJECT,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.wait_for_health()

    def tearDown(self) -> None:
        if self.server.poll() is None:
            self.server.terminate()
        try:
            self.server.wait(timeout=3)
        except subprocess.TimeoutExpired:
            self.server.kill()
            self.server.wait(timeout=3)
        for stream in (self.server.stdout, self.server.stderr):
            if stream is not None:
                stream.close()
        self.temp.cleanup()

    def wait_for_health(self) -> None:
        deadline = time.monotonic() + 5
        last_error: object = None
        while time.monotonic() < deadline:
            if self.server.poll() is not None:
                out, err = self.server.communicate()
                self.fail(f"server exited {self.server.returncode}: stdout={out!r} stderr={err!r}")
            try:
                status, body = self.request("GET", "/healthz")
                if status == 200 and body == {"ok": True}:
                    return
                last_error = (status, body)
            except (HTTPError, URLError, ConnectionError) as exc:
                last_error = exc
            time.sleep(0.05)
        self.fail(f"server did not become healthy: {last_error!r}")

    def request(self, method: str, path: str, payload: object | None = None) -> tuple[int, object]:
        data = None if payload is None else json.dumps(payload).encode("utf-8")
        headers = {} if data is None else {"Content-Type": "application/json"}
        request = Request(self.base + path, data=data, headers=headers, method=method)
        try:
            with urlopen(request, timeout=3) as response:
                raw = response.read()
                return response.status, json.loads(raw) if raw else None
        except HTTPError as exc:
            raw = exc.read()
            result = exc.code, json.loads(raw) if raw else None
            exc.close()
            return result

    def test_http_contract_and_restart_persistence(self) -> None:
        status, body = self.request("GET", "/items")
        self.assertEqual(status, 200)
        self.assertEqual(body, [])

        status, first = self.request(
            "POST",
            "/items",
            {"title": "Read the design", "url": "https://example.test/design", "tags": ["Docs", "rupi", "docs"]},
        )
        self.assertEqual(status, 201)
        self.assertEqual(first["status"], "queued")
        self.assertEqual(first["tags"], ["docs", "rupi"])

        status, second = self.request(
            "POST",
            "/items",
            {"title": "Read the roadmap", "url": "https://example.test/roadmap", "tags": ["rupi"]},
        )
        self.assertEqual(status, 201)
        self.assertEqual(second["id"], first["id"] + 1)

        status, body = self.request("GET", "/items?tag=RUPI")
        self.assertEqual(status, 200)
        self.assertEqual([item["id"] for item in body], [first["id"], second["id"]])

        status, body = self.request("PATCH", f"/items/{first['id']}", {"status": "reading"})
        self.assertEqual(status, 200)
        self.assertEqual(body["status"], "reading")

        status, body = self.request("GET", "/items?status=reading")
        self.assertEqual(status, 200)
        self.assertEqual([item["id"] for item in body], [first["id"]])

        status, body = self.request("POST", "/items", {"title": "", "url": "https://bad.test"})
        self.assertEqual(status, 400)
        self.assertIn("error", body)

        status, body = self.request(
            "POST", "/items", {"title": "Duplicate", "url": "https://example.test/design"}
        )
        self.assertEqual(status, 409)
        self.assertIn("error", body)

        status, body = self.request("DELETE", f"/items/{second['id']}")
        self.assertEqual(status, 204)
        self.assertIsNone(body)
        status, body = self.request("GET", f"/items/{second['id']}")
        self.assertEqual(status, 404)
        self.assertIn("error", body)

        self.server.terminate()
        self.server.wait(timeout=3)
        for stream in (self.server.stdout, self.server.stderr):
            if stream is not None:
                stream.close()
        self.server = subprocess.Popen(
            [
                sys.executable,
                "-m",
                "readqueue",
                "serve",
                "--db",
                str(self.db),
                "--host",
                "127.0.0.1",
                "--port",
                str(self.port),
            ],
            cwd=PROJECT,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.wait_for_health()
        status, body = self.request("GET", "/items")
        self.assertEqual(status, 200)
        self.assertEqual([item["id"] for item in body], [first["id"]])
        self.assertEqual(body[0]["status"], "reading")


if __name__ == "__main__":
    unittest.main()
