from __future__ import annotations

import ast
import hashlib
import hmac
import json
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from urllib import error, request


CASE_ROOT = Path(__file__).resolve().parents[1]
PROJECT_ROOT = CASE_ROOT / "project"
SINK = Path(__file__).with_name("fake_sink.py")
SECRET = "oracle-secret"


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def read_json_response(response) -> tuple[int, dict | list | None]:
    try:
        status = int(response.status)
        body = response.read()
        if not body:
            return status, None
        return status, json.loads(body.decode("utf-8"))
    finally:
        response.close()


def http_call(
    base: str,
    method: str,
    path: str,
    body: bytes | None = None,
    signature: str | None = None,
):
    headers = {"Accept": "application/json"}
    if body is not None:
        headers["Content-Type"] = "application/json"
    if signature is not None:
        headers["X-Pipeline-Signature"] = signature
    req = request.Request(base + path, data=body, headers=headers, method=method)
    try:
        with request.urlopen(req, timeout=3) as response:
            return read_json_response(response)
    except error.HTTPError as exc:
        return read_json_response(exc)


def canonical_body(value: dict) -> bytes:
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode("utf-8")


def signed(value: dict) -> tuple[bytes, str]:
    body = canonical_body(value)
    digest = hmac.new(SECRET.encode("utf-8"), body, hashlib.sha256).hexdigest()
    return body, "sha256=" + digest


def pipeline(pipeline_id: str) -> dict:
    return {
        "pipeline_id": pipeline_id,
        "jobs": [
            {
                "job_id": "source",
                "kind": "source",
                "payload": {"batch": "b-1"},
                "depends_on": [],
                "input_refs": {},
                "collect": None,
            },
            {
                "job_id": "part-a",
                "kind": "part",
                "payload": {"name": "a"},
                "depends_on": ["source"],
                "input_refs": {"source_id": {"job_id": "source", "field": "artifact_id"}},
                "collect": None,
            },
            {
                "job_id": "part-b",
                "kind": "part",
                "payload": {"name": "b"},
                "depends_on": ["source"],
                "input_refs": {"source_id": {"job_id": "source", "field": "artifact_id"}},
                "collect": None,
            },
            {
                "job_id": "barrier",
                "kind": "barrier",
                "payload": {"name": "join"},
                "depends_on": ["part-a", "part-b"],
                "input_refs": {},
                "collect": {"field": "artifact_id", "as": "artifacts"},
            },
            {
                "job_id": "publish",
                "kind": "publish",
                "payload": {"channel": "stable"},
                "depends_on": ["barrier"],
                "input_refs": {"joined": {"job_id": "barrier", "field": "joined"}},
                "collect": None,
            },
        ],
    }


class LeaseCascadeOracle(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="lease-cascade-oracle-")
        self.root = Path(self.temp.name)
        self.db = self.root / "state" / "pipeline.sqlite3"
        self.port = free_port()
        self.base = f"http://127.0.0.1:{self.port}"
        self.server = self.start_server()
        try:
            self.wait_health()
        except BaseException:
            self.stop_server()
            self.temp.cleanup()
            raise

    def tearDown(self) -> None:
        self.stop_server()
        self.temp.cleanup()

    def start_server(self):
        return subprocess.Popen(
            [
                sys.executable,
                "-m",
                "leasecascade",
                "serve",
                "--db",
                str(self.db),
                "--secret",
                SECRET,
                "--host",
                "127.0.0.1",
                "--port",
                str(self.port),
            ],
            cwd=PROJECT_ROOT,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )

    def stop_server(self) -> None:
        server = getattr(self, "server", None)
        if server is None:
            return
        if server.poll() is None:
            server.terminate()
            try:
                server.wait(timeout=3)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait(timeout=3)
        for stream in (server.stdout, server.stderr):
            if stream is not None:
                stream.close()

    def wait_health(self) -> None:
        deadline = time.monotonic() + 8
        last_error = None
        while time.monotonic() < deadline:
            if self.server.poll() is not None:
                stderr = self.server.stderr.read() if self.server.stderr is not None else ""
                self.fail(f"server exited before health check: {stderr.strip()}")
            try:
                status, data = http_call(self.base, "GET", "/healthz")
                if status == 200 and data == {"ok": True}:
                    return
            except (OSError, ValueError) as exc:
                last_error = exc
            time.sleep(0.05)
        self.fail(f"server did not become healthy: {last_error}")

    def post_pipeline(self, document: dict, expected: int | None = None):
        body, signature = signed(document)
        result = http_call(self.base, "POST", "/pipelines", body, signature)
        if expected is not None:
            self.assertEqual(result[0], expected, result)
        return result

    def get_pipeline(self, pipeline_id: str) -> dict:
        status, data = http_call(self.base, "GET", f"/pipelines/{pipeline_id}")
        self.assertEqual(status, 200, data)
        self.assertIsInstance(data, dict)
        return data

    def run_worker(
        self,
        log: Path,
        *sink_extra: str,
        lease_seconds: int = 2,
    ) -> subprocess.CompletedProcess[str]:
        command = [
            sys.executable,
            "-m",
            "leasecascade",
            "worker",
            "--db",
            str(self.db),
            "--sink",
            sys.executable,
        ]
        for arg in [str(SINK), "--log", str(log), *sink_extra]:
            command.append("--sink-arg=" + arg)
        command += ["--lease-seconds", str(lease_seconds), "--once"]
        return subprocess.run(command, cwd=PROJECT_ROOT, capture_output=True, text=True, timeout=10)

    def wait_for(self, predicate, timeout: float = 8) -> None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if predicate():
                return
            time.sleep(0.05)
        self.fail("condition did not become true before timeout")

    def test_fanout_barrier_flow_restart_and_idempotency(self) -> None:
        invalid = pipeline("invalid-collect")
        invalid["jobs"][1]["collect"] = {"field": "artifact_id", "as": "wrong"}
        body, _ = signed(invalid)
        status, data = http_call(self.base, "POST", "/pipelines", body, "sha256=bad")
        self.assertEqual(status, 401)
        self.assertIn("error", data)
        self.assertEqual(http_call(self.base, "GET", "/pipelines/invalid-collect")[0], 404)
        self.assertEqual(self.post_pipeline(invalid, 400)[0], 400)
        self.assertEqual(http_call(self.base, "GET", "/pipelines/invalid-collect")[0], 404)

        document = pipeline("cascade-1")
        first = self.post_pipeline(document, 202)
        self.assertEqual(first[1]["status"], "pending")
        self.assertEqual(first[1]["jobs"][3]["collect"], {"field": "artifact_id", "as": "artifacts"})
        self.assertNotIn(SECRET.encode("utf-8"), self.db.read_bytes())
        self.assertEqual(self.post_pipeline(document, 200)[1], first[1])
        conflict = json.loads(json.dumps(document))
        conflict["jobs"][0]["payload"]["batch"] = "changed"
        self.assertEqual(self.post_pipeline(conflict, 409)[0], 409)

        log = self.root / "ordered.log"
        result = self.run_worker(log)
        self.assertEqual(result.returncode, 0, result.stderr)
        delivered = [json.loads(line) for line in log.read_text(encoding="utf-8").splitlines()]
        self.assertEqual([item["job_id"] for item in delivered], ["source", "part-a", "part-b", "barrier", "publish"])
        self.assertEqual(delivered[1]["inputs"], {"source_id": "artifact-cascade-1-source"})
        self.assertNotIn("private", delivered[1]["inputs"])
        self.assertEqual(
            delivered[3]["fan_in"],
            {
                "as": "artifacts",
                "items": [
                    {"job_id": "part-a", "value": "artifact-cascade-1-part-a"},
                    {"job_id": "part-b", "value": "artifact-cascade-1-part-b"},
                ],
            },
        )
        self.assertEqual(delivered[4]["inputs"], {"joined": [
            "artifact-cascade-1-part-a",
            "artifact-cascade-1-part-b",
        ]})
        state = self.get_pipeline("cascade-1")
        self.assertEqual(state["status"], "succeeded")
        self.assertEqual([job["status"] for job in state["jobs"]], ["succeeded"] * 5)
        self.assertEqual(state["jobs"][3]["output"]["joined_ids"], ["part-a", "part-b"])

        self.stop_server()
        self.server = self.start_server()
        self.wait_health()
        self.assertEqual(self.get_pipeline("cascade-1")["status"], "succeeded")
        before = log.read_text(encoding="utf-8")
        self.assertEqual(self.run_worker(log).returncode, 0)
        self.assertEqual(log.read_text(encoding="utf-8"), before)

    def test_retry_terminal_and_blocked_barrier_cascade(self) -> None:
        retry = pipeline("retry-1")
        self.post_pipeline(retry, 202)
        retry_log = self.root / "retry.log"
        result = self.run_worker(retry_log, "--retry-job", "part-b")
        self.assertNotEqual(result.returncode, 0)
        state = self.get_pipeline("retry-1")
        self.assertEqual([job["status"] for job in state["jobs"]], ["succeeded", "succeeded", "pending", "pending", "pending"])
        result = self.run_worker(retry_log)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.get_pipeline("retry-1")["status"], "succeeded")

        failed = pipeline("failed-1")
        self.post_pipeline(failed, 202)
        failed_log = self.root / "failed.log"
        result = self.run_worker(failed_log, "--fail-job", "part-b")
        self.assertNotEqual(result.returncode, 0)
        state = self.get_pipeline("failed-1")
        self.assertEqual(state["status"], "failed")
        self.assertEqual(
            [job["status"] for job in state["jobs"]],
            ["succeeded", "succeeded", "failed", "blocked", "blocked"],
        )
        delivered_ids = [json.loads(line)["job_id"] for line in failed_log.read_text(encoding="utf-8").splitlines()]
        self.assertEqual(delivered_ids, ["source", "part-a", "part-b"])

    def test_missing_selected_output_fails_barrier_without_sink_call(self) -> None:
        document = pipeline("missing-field-1")
        self.post_pipeline(document, 202)
        log = self.root / "missing.log"
        result = self.run_worker(log, "--omit-field", "part-b")
        self.assertNotEqual(result.returncode, 0)
        delivered_ids = [json.loads(line)["job_id"] for line in log.read_text(encoding="utf-8").splitlines()]
        self.assertEqual(delivered_ids, ["source", "part-a", "part-b"])
        state = self.get_pipeline("missing-field-1")
        self.assertEqual([job["status"] for job in state["jobs"]], ["succeeded", "succeeded", "succeeded", "failed", "blocked"])
        self.assertEqual(state["jobs"][3]["attempts"], 0)
        self.assertTrue(state["jobs"][3]["last_error"])

    def test_crashed_lease_is_reclaimed_by_fresh_worker(self) -> None:
        document = pipeline("crash-1")
        self.post_pipeline(document, 202)
        log = self.root / "crash.log"
        claimed = self.root / "claimed.marker"
        release = self.root / "release.marker"
        command = [
            sys.executable,
            "-m",
            "leasecascade",
            "worker",
            "--db",
            str(self.db),
            "--sink",
            sys.executable,
            "--sink-arg=" + str(SINK),
            "--sink-arg=--log",
            "--sink-arg=" + str(log),
            "--sink-arg=--block-job",
            "--sink-arg=part-a",
            "--sink-arg=--block-file",
            "--sink-arg=" + str(claimed),
            "--sink-arg=--release-file",
            "--sink-arg=" + str(release),
            "--lease-seconds",
            "1",
            "--once",
        ]
        worker = subprocess.Popen(command, cwd=PROJECT_ROOT, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            self.wait_for(claimed.exists)
            self.assertEqual(self.get_pipeline("crash-1")["jobs"][1]["status"], "leased")
        finally:
            worker.terminate()
            worker.wait(timeout=3)
            for stream in (worker.stdout, worker.stderr):
                if stream is not None:
                    stream.close()
        release.write_text("release\n", encoding="utf-8")
        self.wait_for(lambda: self.get_pipeline("crash-1")["jobs"][1]["status"] == "pending", timeout=5)
        result = self.run_worker(log)
        self.assertEqual(result.returncode, 0, result.stderr)
        state = self.get_pipeline("crash-1")
        self.assertEqual(state["status"], "succeeded")
        self.assertEqual(state["jobs"][1]["attempts"], 2)
        delivered_ids = [json.loads(line)["job_id"] for line in log.read_text(encoding="utf-8").splitlines()]
        self.assertEqual(delivered_ids.count("source"), 1)
        self.assertEqual(delivered_ids.count("part-a"), 2)
        self.assertEqual(delivered_ids[-3:], ["part-b", "barrier", "publish"])

    def test_help_commands_and_standard_library_boundary(self) -> None:
        for args in ([], ["serve"], ["worker"]):
            result = subprocess.run(
                [sys.executable, "-m", "leasecascade", *args, "--help"],
                cwd=PROJECT_ROOT,
                capture_output=True,
                text=True,
                timeout=5,
            )
            self.assertEqual(result.returncode, 0, (args, result.stdout, result.stderr))
            self.assertIn("help", (result.stdout + result.stderr).lower())
        allowed = set(getattr(sys, "stdlib_module_names", ())) | {"leasecascade", "tests"}
        for path in PROJECT_ROOT.rglob("*.py"):
            if "__pycache__" in path.parts:
                continue
            tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
            for node in ast.walk(tree):
                if isinstance(node, ast.Import):
                    roots = [alias.name.split(".", 1)[0] for alias in node.names]
                elif isinstance(node, ast.ImportFrom) and node.module is not None and node.level == 0:
                    roots = [node.module.split(".", 1)[0]]
                else:
                    continue
                for root in roots:
                    self.assertIn(root, allowed, f"non-standard import {root!r} in {path}")


if __name__ == "__main__":
    unittest.main()
