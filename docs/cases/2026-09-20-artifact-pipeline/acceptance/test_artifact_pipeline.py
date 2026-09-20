from __future__ import annotations

import hashlib
import hmac
import ast
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


def http_call(base: str, method: str, path: str, body: bytes | None = None, signature: str | None = None):
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


class ArtifactPipelineOracle(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="artifact-pipeline-oracle-")
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
                "artifactpipe",
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
            try:
                status, data = http_call(self.base, "GET", "/healthz")
                if status == 200 and data == {"ok": True}:
                    return
            except (OSError, ValueError) as exc:
                last_error = exc
            time.sleep(0.05)
        self.fail(f"server did not become healthy: {last_error}")

    def post_pipeline(self, pipeline: dict, expected: int | None = None):
        body, signature = signed(pipeline)
        result = http_call(self.base, "POST", "/pipelines", body, signature)
        if expected is not None:
            self.assertEqual(result[0], expected, result)
        return result

    def get_pipeline(self, pipeline_id: str) -> dict:
        status, data = http_call(self.base, "GET", f"/pipelines/{pipeline_id}")
        self.assertEqual(status, 200, data)
        self.assertIsInstance(data, dict)
        return data

    def sink_args(self, log: Path, *extra: str) -> list[str]:
        return ["--log", str(log), *extra]

    def run_worker(self, log: Path, *sink_args: str, lease_seconds: int = 2) -> subprocess.CompletedProcess[str]:
        command = [
            sys.executable,
            "-m",
            "artifactpipe",
            "worker",
            "--db",
            str(self.db),
            "--sink",
            sys.executable,
        ]
        for arg in [str(SINK), *self.sink_args(log, *sink_args)]:
            command.append("--sink-arg=" + arg)
        command += ["--lease-seconds", str(lease_seconds), "--once"]
        return subprocess.run(command, cwd=PROJECT_ROOT, capture_output=True, text=True, timeout=8)

    def test_reference_flow_admission_order_and_restart(self) -> None:
        invalid = {
            "pipeline_id": "bad-ref",
            "jobs": [
                {"job_id": "build", "kind": "build", "payload": {}, "depends_on": [], "input_refs": {}},
                {
                    "job_id": "publish",
                    "kind": "publish",
                    "payload": {},
                    "depends_on": [],
                    "input_refs": {"artifact_id": {"job_id": "build", "field": "artifact_id"}},
                },
            ],
        }
        body, _ = signed(invalid)
        status, data = http_call(self.base, "POST", "/pipelines", body, "sha256=bad")
        self.assertEqual(status, 401)
        self.assertIn("error", data)
        status, data = self.post_pipeline(invalid)
        self.assertEqual(status, 400)
        self.assertIn("error", data)
        self.assertEqual(http_call(self.base, "GET", "/pipelines/bad-ref")[0], 404)
        malformed = b'{"pipeline_id":'
        malformed_signature = "sha256=" + hmac.new(SECRET.encode("utf-8"), malformed, hashlib.sha256).hexdigest()
        status, data = http_call(self.base, "POST", "/pipelines", malformed, malformed_signature)
        self.assertEqual(status, 400)
        self.assertIn("error", data)
        self.assertEqual(http_call(self.base, "GET", "/pipelines/malformed")[0], 404)

        pipeline = {
            "pipeline_id": "release-1",
            "jobs": [
                {
                    "job_id": "build",
                    "kind": "build",
                    "payload": {"version": "1.2.3"},
                    "depends_on": [],
                    "input_refs": {},
                },
                {
                    "job_id": "publish",
                    "kind": "publish",
                    "payload": {"channel": "stable"},
                    "depends_on": ["build"],
                    "input_refs": {"artifact_id": {"job_id": "build", "field": "artifact_id"}},
                },
            ],
        }
        first = self.post_pipeline(pipeline, 202)
        self.assertEqual(first[1]["status"], "pending")
        self.assertIsNone(first[1]["jobs"][0]["output"])
        self.assertNotIn(SECRET.encode("utf-8"), self.db.read_bytes())
        self.assertEqual(self.post_pipeline(pipeline, 200)[1], first[1])
        conflict = json.loads(json.dumps(pipeline))
        conflict["jobs"][0]["payload"]["version"] = "1.2.4"
        self.assertEqual(self.post_pipeline(conflict, 409)[0], 409)

        log = self.root / "ordered.log"
        result = self.run_worker(log)
        self.assertEqual(result.returncode, 0, result.stderr)
        delivered = [json.loads(line) for line in log.read_text(encoding="utf-8").splitlines()]
        self.assertEqual([job["job_id"] for job in delivered], ["build", "publish"])
        self.assertEqual(delivered[1]["inputs"], {"artifact_id": "artifact-release-1-build"})
        state = self.get_pipeline("release-1")
        self.assertEqual(state["status"], "succeeded")
        self.assertEqual([job["status"] for job in state["jobs"]], ["succeeded", "succeeded"])
        self.assertEqual(state["jobs"][0]["output"]["artifact_id"], "artifact-release-1-build")
        self.assertEqual(state["jobs"][1]["output"]["used"], "artifact-release-1-build")

        self.stop_server()
        self.server = self.start_server()
        self.wait_health()
        self.assertEqual(self.get_pipeline("release-1")["status"], "succeeded")

    def test_retryable_terminal_and_blocked_states(self) -> None:
        retry_pipeline = {
            "pipeline_id": "retry-1",
            "jobs": [
                {"job_id": "build", "kind": "build", "payload": {}, "depends_on": [], "input_refs": {}},
                {
                    "job_id": "publish",
                    "kind": "publish",
                    "payload": {},
                    "depends_on": ["build"],
                    "input_refs": {"artifact_id": {"job_id": "build", "field": "artifact_id"}},
                },
            ],
        }
        self.post_pipeline(retry_pipeline, 202)
        log = self.root / "retry.log"
        marker = self.root / "retry.marker"
        result = self.run_worker(log, "--retry-job", "build", "--retry-marker", str(marker))
        self.assertNotEqual(result.returncode, 0)
        state = self.get_pipeline("retry-1")
        self.assertEqual([job["status"] for job in state["jobs"]], ["pending", "pending"])
        self.assertEqual(state["jobs"][0]["attempts"], 1)
        result = self.run_worker(log, "--retry-job", "build", "--retry-marker", str(marker))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.get_pipeline("retry-1")["status"], "succeeded")

        failed = {
            "pipeline_id": "failed-1",
            "jobs": [
                {"job_id": "root", "kind": "root", "payload": {}, "depends_on": [], "input_refs": {}},
                {
                    "job_id": "child",
                    "kind": "child",
                    "payload": {},
                    "depends_on": ["root"],
                    "input_refs": {},
                },
            ],
        }
        self.post_pipeline(failed, 202)
        fail_log = self.root / "failed.log"
        result = self.run_worker(fail_log, "--fail-job", "root")
        self.assertNotEqual(result.returncode, 0)
        state = self.get_pipeline("failed-1")
        self.assertEqual(state["status"], "failed")
        self.assertEqual([job["status"] for job in state["jobs"]], ["failed", "blocked"])
        delivered_ids = [json.loads(line)["job_id"] for line in fail_log.read_text(encoding="utf-8").splitlines()]
        self.assertEqual(delivered_ids, ["root"])

    def test_crashed_lease_is_reclaimed_by_fresh_worker(self) -> None:
        pipeline = {
            "pipeline_id": "crash-1",
            "jobs": [{"job_id": "build", "kind": "build", "payload": {}, "depends_on": [], "input_refs": {}}],
        }
        self.post_pipeline(pipeline, 202)
        log = self.root / "crash.log"
        claimed = self.root / "claimed.marker"
        release = self.root / "release.marker"
        command = [
            sys.executable,
            "-m",
            "artifactpipe",
            "worker",
            "--db",
            str(self.db),
            "--sink",
            sys.executable,
            "--sink-arg=" + str(SINK),
            "--sink-arg=--log",
            "--sink-arg=" + str(log),
            "--sink-arg=--block-file",
            "--sink-arg=" + str(claimed),
            "--sink-arg=--release-file",
            "--sink-arg=" + str(release),
            "--lease-seconds",
            "1",
            "--once",
        ]
        worker = subprocess.Popen(command, cwd=PROJECT_ROOT, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline and not claimed.exists():
            time.sleep(0.03)
        self.assertTrue(claimed.exists(), "worker never reached sink")
        self.assertEqual(self.get_pipeline("crash-1")["jobs"][0]["status"], "leased")
        worker.terminate()
        worker.wait(timeout=3)
        for stream in (worker.stdout, worker.stderr):
            if stream is not None:
                stream.close()
        release.write_text("release\n", encoding="utf-8")
        time.sleep(1.2)
        self.assertEqual(self.get_pipeline("crash-1")["jobs"][0]["status"], "pending")
        result = self.run_worker(log)
        self.assertEqual(result.returncode, 0, result.stderr)
        job = self.get_pipeline("crash-1")["jobs"][0]
        self.assertEqual(job["status"], "succeeded")
        self.assertEqual(job["attempts"], 2)

    def test_help_commands_and_standard_library_boundary(self) -> None:
        for args in ([], ["serve"], ["worker"]):
            result = subprocess.run(
                [sys.executable, "-m", "artifactpipe", *args, "--help"],
                cwd=PROJECT_ROOT,
                capture_output=True,
                text=True,
                timeout=5,
            )
            self.assertEqual(result.returncode, 0, (args, result.stdout, result.stderr))
            self.assertIn("help", (result.stdout + result.stderr).lower())
        allowed = set(getattr(sys, "stdlib_module_names", ())) | {"artifactpipe", "tests"}
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
