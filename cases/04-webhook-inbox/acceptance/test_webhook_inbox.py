"""Fresh-process acceptance oracle for the Webhook Inbox project.

This file deliberately imports no project modules. It treats the project as a
command-line and HTTP black box and uses separate Python processes for the
server, worker, and sink programs.
"""

from __future__ import annotations

import hashlib
import hmac
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from urllib.error import HTTPError
from urllib.request import Request, urlopen


CASE_ROOT = Path(__file__).resolve().parents[1]
PROJECT_ROOT = CASE_ROOT / "project"
FAKE_SINK = Path(__file__).with_name("fake_sink.py")
BLOCKING_SINK = Path(__file__).with_name("blocking_sink.py")
PYTHON = sys.executable
SECRET = "oracle-secret"


def json_bytes(value: object) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode("utf-8")


def signature(body: bytes) -> str:
    digest = hmac.new(SECRET.encode("utf-8"), body, hashlib.sha256).hexdigest()
    return f"sha256={digest}"


def wait_until(predicate, timeout: float = 8.0, interval: float = 0.05) -> None:
    deadline = time.monotonic() + timeout
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        try:
            if predicate():
                return
        except Exception as exc:  # The process may still be starting.
            last_error = exc
        time.sleep(interval)
    if last_error is not None:
        raise AssertionError(f"condition did not become true: {last_error}")
    raise AssertionError("condition did not become true before timeout")


class WebhookInboxOracle(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory(prefix="webhook-inbox-oracle-")
        self.root = Path(self.tempdir.name)
        self.db = self.root / "state.sqlite3"
        self.server = self._start_server()
        wait_until(lambda: self._request("GET", "/healthz")[0] == 200)

    def tearDown(self) -> None:
        if getattr(self, "server", None) is not None:
            self._stop(self.server)
        self.tempdir.cleanup()

    def _start_server(self) -> subprocess.Popen[str]:
        port_file = self.root / "port.txt"
        # Pick a free loopback port without importing the project's server.
        import socket

        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        process = subprocess.Popen(
            [
                PYTHON,
                "-m",
                "webhookinbox",
                "serve",
                "--db",
                str(self.db),
                "--secret",
                SECRET,
                "--host",
                "127.0.0.1",
                "--port",
                str(port),
            ],
            cwd=PROJECT_ROOT,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        self.port = port
        self.base_url = f"http://127.0.0.1:{port}"
        return process

    @staticmethod
    def _stop(process: subprocess.Popen[str]) -> None:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=3)

    def _request(
        self,
        method: str,
        path: str,
        body: bytes | None = None,
        headers: dict[str, str] | None = None,
    ) -> tuple[int, dict[str, object]]:
        request = Request(
            self.base_url + path,
            data=body,
            headers=headers or {},
            method=method,
        )
        try:
            with urlopen(request, timeout=4) as response:
                return int(response.status), json.loads(response.read())
        except HTTPError as error:
            with error:
                return int(error.code), json.loads(error.read())

    def _post(self, value: object, signed: bool = True) -> tuple[int, dict[str, object]]:
        body = json_bytes(value)
        headers = {"Content-Type": "application/json"}
        if signed:
            headers["X-Webhook-Signature"] = signature(body)
        return self._request("POST", "/deliveries", body, headers)

    def _delivery(self, delivery_id: str) -> dict[str, object]:
        status, body = self._request("GET", f"/deliveries/{delivery_id}")
        self.assertEqual(status, 200, body)
        return body

    def _worker(
        self,
        sink: Path,
        sink_args: list[str],
        lease_seconds: int = 30,
    ) -> subprocess.CompletedProcess[str]:
        command = [
            PYTHON,
            "-m",
            "webhookinbox",
            "worker",
            "--db",
            str(self.db),
            "--sink",
            PYTHON,
        ]
        command.extend(["--sink-arg", str(sink)])
        for arg in sink_args:
            # The explicit equals form also proves option-like values remain
            # sink data instead of being consumed by the worker parser.
            command.append(f"--sink-arg={arg}")
        command.extend(
            ["--lease-seconds", str(lease_seconds), "--once"]
        )
        return subprocess.run(
            command,
            cwd=PROJECT_ROOT,
            capture_output=True,
            text=True,
            timeout=20,
        )

    def test_signed_delivery_and_crash_reclaim(self) -> None:
        status, body = self._request("GET", "/deliveries/missing")
        self.assertEqual(status, 404)
        self.assertIn("error", body)

        first = {
            "delivery_id": "del-1",
            "event_type": "release.published",
            "payload": {"version": "0.4.0"},
        }
        status, created = self._post(first)
        self.assertEqual(status, 202, created)
        self.assertEqual(created["status"], "pending")
        self.assertEqual(created["attempts"], 0)
        self.assertIsNone(created["lease_expires_at"])

        status, duplicate = self._post(first)
        self.assertEqual(status, 200, duplicate)
        self.assertEqual(duplicate, created)

        conflict = dict(first)
        conflict["payload"] = {"version": "0.4.1"}
        status, conflict_body = self._post(conflict)
        self.assertEqual(status, 409, conflict_body)
        self.assertIn("error", conflict_body)
        self.assertEqual(self._delivery("del-1"), created)

        invalid_body = json_bytes(
            {"delivery_id": "bad", "event_type": "bad", "payload": {}}
        )
        status, invalid = self._request(
            "POST",
            "/deliveries",
            invalid_body,
            {"Content-Type": "application/json", "X-Webhook-Signature": "sha256=bad"},
        )
        self.assertEqual(status, 401, invalid)
        malformed = b"{not-json"
        status, malformed_body = self._request(
            "POST",
            "/deliveries",
            malformed,
            {"Content-Type": "application/json", "X-Webhook-Signature": signature(malformed)},
        )
        self.assertEqual(status, 400, malformed_body)
        self.assertEqual(self._delivery("del-1"), created)

        log_path = self.root / "sink.log"
        result = self._worker(FAKE_SINK, ["--log", str(log_path), "--tag", "literal;data"])
        self.assertEqual(result.returncode, 0, result.stderr)
        delivered = self._delivery("del-1")
        self.assertEqual(delivered["status"], "delivered")
        self.assertEqual(delivered["attempts"], 1)
        self.assertIsNone(delivered["lease_expires_at"])
        self.assertEqual(delivered["last_error"], None)
        sink_records = [json.loads(line) for line in log_path.read_text().splitlines()]
        self.assertEqual(sink_records[0]["tag"], "literal;data")
        self.assertEqual(sink_records[0]["request"]["delivery_id"], "del-1")

        second = {
            "delivery_id": "del-2",
            "event_type": "build.finished",
            "payload": {"build": 7},
        }
        status, _ = self._post(second)
        self.assertEqual(status, 202)
        marker = self.root / "claimed.json"
        release = self.root / "release.flag"
        blocking = subprocess.Popen(
            [
                PYTHON,
                "-m",
                "webhookinbox",
                "worker",
                "--db",
                str(self.db),
                "--sink",
                PYTHON,
                "--sink-arg",
                str(BLOCKING_SINK),
                "--sink-arg=--marker",
                "--sink-arg=" + str(marker),
                "--sink-arg=--release",
                "--sink-arg=" + str(release),
                "--lease-seconds",
                "1",
                "--once",
            ],
            cwd=PROJECT_ROOT,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            text=True,
        )
        try:
            wait_until(marker.exists)
            leased = self._delivery("del-2")
            self.assertEqual(leased["status"], "leased")
            self.assertEqual(leased["attempts"], 1)
            self.assertIsInstance(leased["lease_expires_at"], int)
            blocking.kill()
            blocking.wait(timeout=3)
        finally:
            release.write_text("release", encoding="utf-8")
            if blocking.poll() is None:
                blocking.kill()
                blocking.wait(timeout=3)

        def is_pending() -> bool:
            return self._delivery("del-2")["status"] == "pending"

        wait_until(is_pending, timeout=5)
        result = self._worker(FAKE_SINK, ["--log", str(log_path), "--tag", "reclaimed"])
        self.assertEqual(result.returncode, 0, result.stderr)
        reclaimed = self._delivery("del-2")
        self.assertEqual(reclaimed["status"], "delivered")
        self.assertEqual(reclaimed["attempts"], 2)

        self._stop(self.server)
        self.server = self._start_server()
        wait_until(lambda: self._request("GET", "/healthz")[0] == 200)
        self.assertEqual(self._delivery("del-1")["status"], "delivered")
        self.assertEqual(self._delivery("del-2")["attempts"], 2)

    def test_help_commands(self) -> None:
        for args in (["--help"], ["serve", "--help"], ["worker", "--help"]):
            result = subprocess.run(
                [PYTHON, "-m", "webhookinbox", *args],
                cwd=PROJECT_ROOT,
                capture_output=True,
                text=True,
                timeout=5,
            )
            self.assertEqual(result.returncode, 0, (args, result.stderr))
            self.assertTrue(result.stdout.strip(), args)


if __name__ == "__main__":
    unittest.main()
