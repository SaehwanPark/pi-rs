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


def single_pipeline(pipeline_id: str, with_child: bool = False) -> dict:
    jobs = [
        {
            "job_id": "source",
            "kind": "source",
            "payload": {"batch": pipeline_id},
            "depends_on": [],
            "input_refs": {},
            "collect": None,
        }
    ]
    if with_child:
        jobs.append(
            {
                "job_id": "child",
                "kind": "child",
                "payload": {"name": "child"},
                "depends_on": ["source"],
                "input_refs": {"source_id": {"job_id": "source", "field": "artifact_id"}},
                "collect": None,
            }
        )
    return {"pipeline_id": pipeline_id, "jobs": jobs}


def start_service(db: Path) -> tuple[subprocess.Popen[str], str]:
    port = free_port()
    process = subprocess.Popen(
        [
            sys.executable,
            "-m",
            "leasereceipt",
            "serve",
            "--db",
            str(db),
            "--secret",
            SECRET,
            "--host",
            "127.0.0.1",
            "--port",
            str(port),
        ],
        cwd=PROJECT_ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    base = f"http://127.0.0.1:{port}"
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        if process.poll() is not None:
            stderr = process.stderr.read() if process.stderr else ""
            if process.stdout:
                process.stdout.close()
            if process.stderr:
                process.stderr.close()
            raise AssertionError(f"service exited before health check: {stderr}")
        try:
            status, body = http_call(base, "GET", "/healthz")
            if status == 200 and body == {"ok": True}:
                return process, base
        except OSError:
            pass
        time.sleep(0.05)
    stop_process(process)
    raise AssertionError("service did not become healthy")


def stop_process(process: subprocess.Popen[str]) -> None:
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=3)
    if process.stdout:
        process.stdout.close()
    if process.stderr:
        process.stderr.close()


def worker_command(
    db: Path,
    label: str,
    log: Path,
    receipts: Path,
    *,
    lease_seconds: int = 30,
    block_before_apply_job: str | None = None,
    block_after_apply_job: str | None = None,
    claimed_file: Path | None = None,
    applied_file: Path | None = None,
    done_file: Path | None = None,
    release_file: Path | None = None,
    retry_job: str | None = None,
    fail_job: str | None = None,
) -> list[str]:
    sink_args = [str(SINK), "--log", str(log), "--label", label, "--receipts", str(receipts)]
    if block_before_apply_job is not None:
        sink_args.extend(["--block-before-apply-job", block_before_apply_job])
    if block_after_apply_job is not None:
        sink_args.extend(["--block-after-apply-job", block_after_apply_job])
    if claimed_file is not None:
        sink_args.extend(["--claimed-file", str(claimed_file)])
    if applied_file is not None:
        sink_args.extend(["--applied-file", str(applied_file)])
    if done_file is not None:
        sink_args.extend(["--done-file", str(done_file)])
    if release_file is not None:
        sink_args.extend(["--release-file", str(release_file)])
    if retry_job is not None:
        sink_args.extend(["--retry-job", retry_job])
    if fail_job is not None:
        sink_args.extend(["--fail-job", fail_job])
    command = [
        sys.executable,
        "-m",
        "leasereceipt",
        "worker",
        "--db",
        str(db),
        "--sink",
        sys.executable,
    ]
    command.extend("--sink-arg=" + value for value in sink_args)
    command.extend(["--lease-seconds", str(lease_seconds), "--once"])
    return command


def run_worker(command: list[str], timeout: float = 12) -> tuple[int, str, str]:
    process = subprocess.Popen(
        command,
        cwd=PROJECT_ROOT,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        process.kill()
        stdout, stderr = process.communicate()
        raise AssertionError(f"worker timed out: stdout={stdout!r} stderr={stderr!r}")
    return int(process.returncode), stdout, stderr


def read_log(path: Path) -> list[dict]:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]


def wait_for(predicate, timeout: float = 6) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.05)
    raise AssertionError("condition did not become true before timeout")


class LeaseReceiptOracle(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="lease-receipt-oracle-")
        self.root = Path(self.temp.name)
        self.db = self.root / "state" / "pipeline.sqlite3"
        self.service: subprocess.Popen[str] | None = None

    def tearDown(self) -> None:
        if self.service is not None:
            stop_process(self.service)
        self.temp.cleanup()

    def start(self) -> str:
        self.service, base = start_service(self.db)
        return base

    def admit(self, base: str, value: dict) -> tuple[int, dict | list | None]:
        body, signature = signed(value)
        return http_call(base, "POST", "/pipelines", body, signature)

    def test_atomic_admission_order_data_flow_and_restart(self) -> None:
        base = self.start()
        value = pipeline("ordered")
        body, _signature = signed(value)

        status, _ = http_call(base, "POST", "/pipelines", body, "sha256=bad")
        self.assertEqual(status, 401)
        status, _ = http_call(base, "GET", "/pipelines/ordered")
        self.assertEqual(status, 404)

        malformed = b"not-json"
        bad_digest = "sha256=" + hmac.new(SECRET.encode(), malformed, hashlib.sha256).hexdigest()
        status, _ = http_call(base, "POST", "/pipelines", malformed, bad_digest)
        self.assertEqual(status, 400)

        status, admitted = self.admit(base, value)
        self.assertEqual(status, 202)
        self.assertEqual(admitted["status"], "pending")
        status, repeat = self.admit(base, value)
        self.assertEqual(status, 200)
        self.assertEqual(repeat["pipeline_id"], "ordered")

        conflict = json.loads(json.dumps(value))
        conflict["jobs"][0]["payload"]["batch"] = "changed"
        status, _ = self.admit(base, conflict)
        self.assertEqual(status, 409)

        cycle = {
            "pipeline_id": "cycle",
            "jobs": [
                {"job_id": "a", "kind": "a", "payload": {}, "depends_on": ["b"], "input_refs": {}, "collect": None},
                {"job_id": "b", "kind": "b", "payload": {}, "depends_on": ["a"], "input_refs": {}, "collect": None},
            ],
        }
        status, _ = self.admit(base, cycle)
        self.assertEqual(status, 400)
        status, _ = http_call(base, "GET", "/pipelines/cycle")
        self.assertEqual(status, 404)

        log = self.root / "ordered.log"
        receipts = self.root / "ordered.receipts.json"
        code, _stdout, stderr = run_worker(
            worker_command(self.db, "ordered-worker", log, receipts)
        )
        self.assertEqual(code, 0, stderr)
        entries = read_log(log)
        self.assertEqual([entry["job_id"] for entry in entries], ["source", "part-a", "part-b", "barrier", "publish"])
        self.assertNotIn("lease_token", json.dumps(entries))
        self.assertNotIn("claim_token", json.dumps(entries))
        self.assertEqual(len({entry["delivery_key"] for entry in entries}), 5)
        barrier = next(entry for entry in entries if entry["job_id"] == "barrier")
        self.assertEqual([item["job_id"] for item in barrier["fan_in"]["items"]], ["part-a", "part-b"])
        self.assertEqual([item["value"] for item in barrier["fan_in"]["items"]], [
            "artifact-ordered-part-a",
            "artifact-ordered-part-b",
        ])
        self.assertEqual(set(barrier["fan_in"]["items"][0]), {"job_id", "value"})
        publish = next(entry for entry in entries if entry["job_id"] == "publish")
        self.assertEqual(publish["inputs"], {"joined": [
            "artifact-ordered-part-a",
            "artifact-ordered-part-b",
        ]})

        status, result = http_call(base, "GET", "/pipelines/ordered")
        self.assertEqual(status, 200)
        self.assertEqual(result["status"], "succeeded")
        self.assertTrue(all(job["status"] == "succeeded" for job in result["jobs"]))
        self.assertTrue(all(job["receipt"]["delivery_key"] == job["delivery_key"] for job in result["jobs"]))
        self.assertNotIn("lease_token", json.dumps(result))
        self.assertNotIn("claim_token", json.dumps(result))

        stop_process(self.service)
        self.service = None
        self.service, restarted_base = start_service(self.db)
        status, restarted = http_call(restarted_base, "GET", "/pipelines/ordered")
        self.assertEqual(status, 200)
        self.assertEqual(restarted, result)

    def test_retryable_terminal_and_blocked_states(self) -> None:
        base = self.start()
        retry_value = single_pipeline("retry", with_child=True)
        status, _ = self.admit(base, retry_value)
        self.assertEqual(status, 202)
        retry_log = self.root / "retry.log"
        retry_receipts = self.root / "retry.receipts.json"
        code, _stdout, stderr = run_worker(
            worker_command(self.db, "retry-first", retry_log, retry_receipts, retry_job="source")
        )
        self.assertEqual(code, 1, stderr)
        status, pending = http_call(base, "GET", "/pipelines/retry")
        self.assertEqual(status, 200)
        self.assertEqual(pending["jobs"][0]["status"], "pending")
        self.assertEqual(pending["jobs"][0]["attempts"], 1)
        first_key = read_log(retry_log)[0]["delivery_key"]

        code, _stdout, stderr = run_worker(
            worker_command(self.db, "retry-second", retry_log, retry_receipts)
        )
        self.assertEqual(code, 0, stderr)
        retry_entries = read_log(retry_log)
        self.assertEqual([entry["delivery_key"] for entry in retry_entries[:2]], [first_key, first_key])
        self.assertEqual(retry_entries[2]["job_id"], "child")
        status, completed = http_call(base, "GET", "/pipelines/retry")
        self.assertEqual(status, 200)
        self.assertEqual(completed["status"], "succeeded")

        failed_value = single_pipeline("terminal", with_child=True)
        status, _ = self.admit(base, failed_value)
        self.assertEqual(status, 202)
        fail_log = self.root / "fail.log"
        fail_receipts = self.root / "fail.receipts.json"
        code, _stdout, stderr = run_worker(
            worker_command(self.db, "terminal", fail_log, fail_receipts, fail_job="source")
        )
        self.assertEqual(code, 1, stderr)
        status, failed = http_call(base, "GET", "/pipelines/terminal")
        self.assertEqual(status, 200)
        self.assertEqual(failed["status"], "failed")
        self.assertEqual(failed["jobs"][0]["status"], "failed")
        self.assertEqual(failed["jobs"][1]["status"], "blocked")
        self.assertEqual(len(read_log(fail_log)), 1)

    def test_stale_worker_cannot_overwrite_newer_claim(self) -> None:
        base = self.start()
        status, _ = self.admit(base, single_pipeline("fenced"))
        self.assertEqual(status, 202)
        first_log = self.root / "first.log"
        receipts = self.root / "fenced.receipts.json"
        claimed = self.root / "claimed.flag"
        release = self.root / "release.flag"
        first = subprocess.Popen(
            worker_command(
                self.db,
                "stale-a",
                first_log,
                receipts,
                lease_seconds=1,
                block_before_apply_job="source",
                claimed_file=claimed,
                release_file=release,
            ),
            cwd=PROJECT_ROOT,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        try:
            wait_for(claimed.exists)
            wait_for(lambda: http_call(base, "GET", "/pipelines/fenced")[1]["jobs"][0]["status"] == "pending", 5)

            second_log = self.root / "second.log"
            code, _stdout, stderr = run_worker(
                worker_command(self.db, "winner-b", second_log, receipts, lease_seconds=1)
            )
            self.assertEqual(code, 0, stderr)
            status, winner = http_call(base, "GET", "/pipelines/fenced")
            self.assertEqual(status, 200)
            self.assertEqual(winner["status"], "succeeded")
            self.assertEqual(winner["jobs"][0]["attempts"], 2)
            self.assertEqual(winner["jobs"][0]["output"]["worker_label"], "winner-b")

            release.write_text("release\n", encoding="utf-8")
            stale_stdout, stale_stderr = first.communicate(timeout=8)
            self.assertNotEqual(first.returncode, 0, stale_stdout)
            self.assertIn("stale", stale_stderr.lower())

            status, unchanged = http_call(base, "GET", "/pipelines/fenced")
            self.assertEqual(status, 200)
            self.assertEqual(unchanged, winner)
            first_entries = read_log(first_log)
            second_entries = read_log(second_log)
            self.assertEqual(first_entries[0]["delivery_key"], second_entries[0]["delivery_key"])
            self.assertFalse(first_entries[0]["applied"])
            self.assertTrue(second_entries[0]["applied"])
        finally:
            if first.poll() is None:
                release.write_text("release\n", encoding="utf-8")
                try:
                    first.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    first.kill()
                    first.wait(timeout=3)
            if first.stdout:
                first.stdout.close()
            if first.stderr:
                first.stderr.close()

    def test_lost_ack_replays_one_receipt_after_worker_crash(self) -> None:
        base = self.start()
        status, _ = self.admit(base, single_pipeline("receipt"))
        self.assertEqual(status, 202)
        receipts = self.root / "receipt-store.json"
        first_log = self.root / "receipt-first.log"
        applied = self.root / "applied.flag"
        release = self.root / "release.flag"
        done = self.root / "done.flag"
        first = subprocess.Popen(
            worker_command(
                self.db,
                "crash-a",
                first_log,
                receipts,
                lease_seconds=1,
                block_after_apply_job="source",
                applied_file=applied,
                done_file=done,
                release_file=release,
            ),
            cwd=PROJECT_ROOT,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        try:
            wait_for(applied.exists)
            first.kill()
            first.communicate(timeout=5)
            wait_for(lambda: http_call(base, "GET", "/pipelines/receipt")[1]["jobs"][0]["status"] == "pending", 5)

            second_log = self.root / "receipt-second.log"
            code, _stdout, stderr = run_worker(
                worker_command(self.db, "recovery-b", second_log, receipts, lease_seconds=1)
            )
            self.assertEqual(code, 0, stderr)
            status, result = http_call(base, "GET", "/pipelines/receipt")
            self.assertEqual(status, 200)
            self.assertEqual(result["status"], "succeeded")
            job = result["jobs"][0]
            self.assertEqual(job["attempts"], 2)
            self.assertEqual(job["receipt"]["delivery_key"], "receipt:source")
            self.assertEqual(job["receipt"]["receipt_id"], "receipt-receipt-source")

            first_entries = read_log(first_log)
            second_entries = read_log(second_log)
            self.assertEqual(len(first_entries), 1)
            self.assertEqual(len(second_entries), 1)
            self.assertEqual(first_entries[0]["delivery_key"], second_entries[0]["delivery_key"])
            self.assertTrue(first_entries[0]["applied"])
            self.assertFalse(first_entries[0]["replayed"])
            self.assertFalse(second_entries[0]["applied"])
            self.assertTrue(second_entries[0]["replayed"])
            self.assertNotIn("lease_token", json.dumps(first_entries + second_entries))
            self.assertNotIn("claim_token", json.dumps(first_entries + second_entries))
        finally:
            release.write_text("release\n", encoding="utf-8")
            if first.poll() is None:
                first.kill()
                first.wait(timeout=5)
            if first.stdout:
                first.stdout.close()
            if first.stderr:
                first.stderr.close()

    def test_help_and_standard_library_boundary(self) -> None:
        for arguments in (["--help"], ["serve", "--help"], ["worker", "--help"]):
            completed = subprocess.run(
                [sys.executable, "-m", "leasereceipt", *arguments],
                cwd=PROJECT_ROOT,
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertIn("usage:", completed.stdout.lower())

        allowed = set(getattr(sys, "stdlib_module_names", ())) | {"leasereceipt"}
        forbidden: list[str] = []
        for path in PROJECT_ROOT.rglob("*.py"):
            if "__pycache__" in path.parts:
                continue
            tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
            for node in ast.walk(tree):
                if isinstance(node, ast.Import):
                    names = [alias.name for alias in node.names]
                elif isinstance(node, ast.ImportFrom) and node.level == 0 and node.module:
                    names = [node.module]
                else:
                    continue
                for name in names:
                    if name.split(".", 1)[0] not in allowed:
                        forbidden.append(f"{path.relative_to(PROJECT_ROOT)}: {name}")
        self.assertEqual(forbidden, [])


if __name__ == "__main__":
    unittest.main()
