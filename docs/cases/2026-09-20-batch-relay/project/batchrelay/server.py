from __future__ import annotations

import hmac
import json
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import unquote, urlsplit

from .storage import BatchConflictError, Store
from .validation import ValidationError, validate_batch


class RelayHTTPServer(ThreadingHTTPServer):
    allow_reuse_address = True

    def __init__(self, address, secret: str, store: Store):
        self.secret = secret
        self.store = store
        super().__init__(address, RelayHandler)

    def server_close(self) -> None:
        super().server_close()
        self.store.close()


class RelayHandler(BaseHTTPRequestHandler):
    server: RelayHTTPServer

    def log_message(self, _format: str, *_args) -> None:
        return

    def _send(self, status: int, value) -> None:
        if value is None:
            body = b""
        else:
            body = json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        if body:
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
        else:
            self.send_header("Content-Length", "0")
        self.send_header("Connection", "close")
        self.end_headers()
        if body:
            self.wfile.write(body)

    def _error(self, status: int, message: str) -> None:
        self._send(status, {"error": message})

    def _batch_id(self) -> str | None:
        parts = [unquote(part) for part in urlsplit(self.path).path.split("/") if part]
        if len(parts) == 2 and parts[0] == "batches":
            return parts[1]
        return None

    def do_GET(self) -> None:
        path = urlsplit(self.path).path
        if path == "/healthz":
            self._send(HTTPStatus.OK, {"ok": True})
            return
        batch_id = self._batch_id()
        if batch_id is None:
            self._error(HTTPStatus.NOT_FOUND, "unknown route")
            return
        batch = self.server.store.get(batch_id)
        if batch is None:
            self._error(HTTPStatus.NOT_FOUND, "unknown batch")
            return
        self._send(HTTPStatus.OK, batch)

    def do_POST(self) -> None:
        if urlsplit(self.path).path != "/batches":
            self._error(HTTPStatus.NOT_FOUND, "unknown route")
            return
        try:
            length = int(self.headers.get("Content-Length", "-1"))
        except ValueError:
            length = -1
        if length < 0 or length > 2_000_000:
            self._error(HTTPStatus.BAD_REQUEST, "invalid request body length")
            return
        body = self.rfile.read(length)
        expected = "sha256=" + hmac.new(
            self.server.secret.encode("utf-8"), body, "sha256"
        ).hexdigest()
        if not hmac.compare_digest(self.headers.get("X-Batch-Signature", ""), expected):
            self._error(HTTPStatus.UNAUTHORIZED, "invalid batch signature")
            return
        try:
            value = json.loads(body.decode("utf-8"))
            batch = validate_batch(value)
        except (UnicodeDecodeError, json.JSONDecodeError, ValidationError) as exc:
            self._error(HTTPStatus.BAD_REQUEST, str(exc))
            return
        try:
            stored, created = self.server.store.submit(batch)
        except BatchConflictError as exc:
            self._error(HTTPStatus.CONFLICT, str(exc))
            return
        self._send(HTTPStatus.ACCEPTED if created else HTTPStatus.OK, stored)

    def do_PUT(self) -> None:
        self._error(HTTPStatus.METHOD_NOT_ALLOWED, "method not allowed")

    def do_PATCH(self) -> None:
        self._error(HTTPStatus.METHOD_NOT_ALLOWED, "method not allowed")

    def do_DELETE(self) -> None:
        self._error(HTTPStatus.METHOD_NOT_ALLOWED, "method not allowed")


def serve(db: str, secret: str, host: str, port: int) -> int:
    store = Store(db)
    server = RelayHTTPServer((host, port), secret, store)
    print(f"batchrelay listening on http://{host}:{port}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        return 0
    finally:
        server.server_close()
    return 0
