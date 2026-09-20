"""Independent fresh-process acceptance oracle for Event Outbox."""

from __future__ import annotations

import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen


CASE_ROOT = Path(__file__).resolve().parents[1]
PROJECT = CASE_ROOT / "project"
SINK = Path(__file__).with_name("fake_sink.py")


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


class EventOutboxAcceptance(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.db = self.root / "state" / "events.sqlite3"
        self.log = self.root / "sink" / "received.ndjson"
        self.port = free_port()
        self.base = f"http://127.0.0.1:{self.port}"
        self.server: subprocess.Popen[str] | None = None
        self.start_server()
        self.addCleanup(self.stop_server)

    def tearDown(self) -> None:
        self.temp.cleanup()

    def environment(self) -> dict[str, str]:
        env = os.environ.copy()
        env.pop("PYTHONPATH", None)
        return env

    def start_server(self) -> None:
        self.server = subprocess.Popen(
            [
                sys.executable,
                "-m",
                "outbox",
                "serve",
                "--db",
                str(self.db),
                "--host",
                "127.0.0.1",
                "--port",
                str(self.port),
            ],
            cwd=PROJECT,
            env=self.environment(),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.wait_for_health()

    def stop_server(self) -> None:
        if self.server is None:
            return
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
        self.server = None

    def wait_for_health(self) -> None:
        deadline = time.monotonic() + 5
        last_error: object = None
        while time.monotonic() < deadline:
            if self.server is not None and self.server.poll() is not None:
                out, err = self.server.communicate()
                self.fail(
                    f"server exited {self.server.returncode}: stdout={out!r} stderr={err!r}"
                )
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

    def run_worker(self, fail_event: str | None = None) -> subprocess.CompletedProcess[str]:
        args = [
            sys.executable,
            "-m",
            "outbox",
            "worker",
            "--db",
            str(self.db),
            "--sink",
            sys.executable,
            "--sink-arg",
            str(SINK),
            "--sink-arg",
            "--log",
            "--sink-arg",
            str(self.log),
            "--once",
        ]
        if fail_event is not None:
            args.extend(["--sink-arg", "--fail-event", "--sink-arg", fail_event])
        return subprocess.run(
            args,
            cwd=PROJECT,
            env=self.environment(),
            text=True,
            capture_output=True,
            check=False,
            timeout=10,
        )

    def event(self, event_id: str) -> dict[str, object]:
        status, body = self.request("GET", f"/events/{event_id}")
        self.assertEqual(status, 200, body)
        self.assertIsInstance(body, dict)
        return body  # type: ignore[return-value]

    def test_http_idempotency_delivery_retry_and_restart(self) -> None:
        status, body = self.request("GET", "/events/missing")
        self.assertEqual(status, 404)
        self.assertIn("error", body)

        first_payload = {"version": "0.3.0", "files": 3}
        status, first = self.request(
            "POST",
            "/events",
            {"event_id": "evt-1", "topic": "release", "payload": first_payload},
        )
        self.assertEqual(status, 202, first)
        self.assertEqual(first["status"], "pending")
        self.assertEqual(first["attempts"], 0)

        status, duplicate = self.request(
            "POST",
            "/events",
            {"event_id": "evt-1", "topic": "release", "payload": first_payload},
        )
        self.assertEqual(status, 200)
        self.assertEqual(duplicate, first)

        status, conflict = self.request(
            "POST",
            "/events",
            {"event_id": "evt-1", "topic": "release", "payload": {"version": "other"}},
        )
        self.assertEqual(status, 409)
        self.assertIn("error", conflict)
        self.assertEqual(self.event("evt-1")["attempts"], 0)

        status, second = self.request(
            "POST",
            "/events",
            {"event_id": "evt-2", "topic": "release", "payload": {"version": "0.3.1"}},
        )
        self.assertEqual(status, 202)
        self.assertEqual(second["event_id"], "evt-2")

        status, invalid = self.request(
            "POST",
            "/events",
            {"event_id": "bad id", "topic": "release", "payload": {}},
        )
        self.assertEqual(status, 400)
        self.assertIn("error", invalid)
        status, invalid_json = self.request("POST", "/events", {"event_id": "evt-3"})
        self.assertEqual(status, 400)
        self.assertIn("error", invalid_json)
        self.assertEqual(self.event("evt-1")["attempts"], 0)

        delivered = self.run_worker()
        self.assertEqual(delivered.returncode, 0, delivered.stderr)
        first_state = self.event("evt-1")
        self.assertEqual(first_state["status"], "delivered")
        self.assertEqual(first_state["attempts"], 1)
        self.assertIsNone(first_state["last_error"])

        failed = self.run_worker(fail_event="evt-2")
        self.assertNotEqual(failed.returncode, 0)
        failed_state = self.event("evt-2")
        self.assertEqual(failed_state["status"], "pending")
        self.assertEqual(failed_state["attempts"], 1)
        self.assertTrue(failed_state["last_error"])

        retried = self.run_worker()
        self.assertEqual(retried.returncode, 0, retried.stderr)
        retried_state = self.event("evt-2")
        self.assertEqual(retried_state["status"], "delivered")
        self.assertEqual(retried_state["attempts"], 2)

        records = [json.loads(line) for line in self.log.read_text(encoding="utf-8").splitlines()]
        self.assertEqual([record["event_id"] for record in records], ["evt-1", "evt-2", "evt-2"])
        self.assertEqual(records[0]["payload"], first_payload)

        self.stop_server()
        self.start_server()
        self.assertEqual(self.event("evt-1")["status"], "delivered")
        self.assertEqual(self.event("evt-2")["attempts"], 2)

    def test_help_commands(self) -> None:
        for arguments in (("--help",), ("serve", "--help"), ("worker", "--help")):
            result = subprocess.run(
                [sys.executable, "-m", "outbox", *arguments],
                cwd=PROJECT,
                env=self.environment(),
                text=True,
                capture_output=True,
                check=False,
                timeout=5,
            )
            self.assertEqual(result.returncode, 0, (arguments, result.stdout, result.stderr))


if __name__ == "__main__":
    unittest.main()
