"""HTTP boundary for signed pipeline admission and durable inspection."""

from __future__ import annotations

import hashlib
import hmac
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any
from urllib.parse import unquote, urlsplit

from .ids import ValidationError
from .storage import PipelineNotFound, Store


class PipelineHTTPServer(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, address: tuple[str, int], db: str | Path, secret: str) -> None:
        self.store = Store(db)
        self.secret = secret.encode("utf-8")
        super().__init__(address, PipelineHandler)


class PipelineHandler(BaseHTTPRequestHandler):
    server: PipelineHTTPServer

    def do_GET(self) -> None:
        parts = [unquote(part) for part in urlsplit(self.path).path.split("/") if part]
        if parts == ["healthz"]:
            self._send(200, {"ok": True})
            return
        if len(parts) == 2 and parts[0] == "pipelines":
            try:
                self._send(200, self.server.store.read(parts[1]))
            except PipelineNotFound:
                self._send(404, {"error": "unknown pipeline"})
            return
        self._send(404, {"error": "not found"})

    def do_POST(self) -> None:
        parts = [unquote(part) for part in urlsplit(self.path).path.split("/") if part]
        if parts != ["pipelines"]:
            self._send(404, {"error": "not found"})
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
        except ValueError:
            self._send(400, {"error": "invalid content length"})
            return
        if length < 0 or length > 4 * 1024 * 1024:
            self._send(400, {"error": "invalid request size"})
            return
        body = self.rfile.read(length)
        supplied = self.headers.get("X-Pipeline-Signature", "")
        expected = "sha256=" + hmac.new(self.server.secret, body, hashlib.sha256).hexdigest()
        if not supplied or not hmac.compare_digest(supplied, expected):
            self._send(401, {"error": "invalid pipeline signature"})
            return
        try:
            document = json.loads(body.decode("utf-8"))
            admission = self.server.store.admit(document)
        except (UnicodeDecodeError, json.JSONDecodeError, ValidationError) as exc:
            self._send(400, {"error": str(exc)})
            return
        self._send(admission.status, admission.pipeline)

    def do_PUT(self) -> None:
        self._send(405, {"error": "method not allowed"})

    do_DELETE = do_PUT
    do_PATCH = do_PUT

    def _send(self, status: int, value: Any) -> None:
        body = json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format: str, *args: Any) -> None:
        return


def serve(db: str, secret: str, host: str, port: int) -> None:
    server = PipelineHTTPServer((host, port), db, secret)
    try:
        server.serve_forever()
    finally:
        server.server_close()
