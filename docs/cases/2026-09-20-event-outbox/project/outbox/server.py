from __future__ import annotations

import json
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import PurePosixPath
import sys
from urllib.parse import unquote, urlsplit

from .model import ValidationError, normalize_event
from .storage import Store


class OutboxHandler(BaseHTTPRequestHandler):
    server: "OutboxHTTPServer"

    def do_GET(self) -> None:  # noqa: N802
        parsed = urlsplit(self.path)
        if parsed.path == "/healthz":
            self._json(HTTPStatus.OK, {"ok": True})
            return
        parts = PurePosixPath(parsed.path).parts
        if len(parts) != 3 or parts[1] != "events":
            self._error(HTTPStatus.NOT_FOUND, "unknown path")
            return
        event = self.server.store.get(unquote(parts[2]))
        if event is None:
            self._error(HTTPStatus.NOT_FOUND, "event not found")
        else:
            self._json(HTTPStatus.OK, event)

    def do_POST(self) -> None:  # noqa: N802
        if urlsplit(self.path).path != "/events":
            self._error(HTTPStatus.NOT_FOUND, "unknown path")
            return
        try:
            event = normalize_event(self._body())
        except (ValidationError, json.JSONDecodeError, UnicodeDecodeError) as exc:
            self._error(HTTPStatus.BAD_REQUEST, str(exc))
            return
        outcome, stored = self.server.store.add(event)
        if outcome == "conflict":
            self._error(HTTPStatus.CONFLICT, "event_id already contains different data")
        elif outcome == "duplicate":
            self._json(HTTPStatus.OK, stored)
        else:
            self._json(HTTPStatus.ACCEPTED, stored)

    def do_PUT(self) -> None:  # noqa: N802
        self._error(HTTPStatus.METHOD_NOT_ALLOWED, "method not allowed")

    def do_PATCH(self) -> None:  # noqa: N802
        self._error(HTTPStatus.METHOD_NOT_ALLOWED, "method not allowed")

    def do_DELETE(self) -> None:  # noqa: N802
        self._error(HTTPStatus.METHOD_NOT_ALLOWED, "method not allowed")

    def do_HEAD(self) -> None:  # noqa: N802
        self._error(HTTPStatus.METHOD_NOT_ALLOWED, "method not allowed", write_body=False)

    def do_OPTIONS(self) -> None:  # noqa: N802
        self._error(HTTPStatus.METHOD_NOT_ALLOWED, "method not allowed")

    def send_error(
        self, code: int, message: str | None = None, explain: str | None = None
    ) -> None:
        del explain
        try:
            status = HTTPStatus(code)
        except ValueError:
            status = HTTPStatus.BAD_REQUEST
        self._error(status, message or status.phrase, write_body=self.command != "HEAD")

    def _body(self) -> object:
        length_text = self.headers.get("Content-Length")
        if length_text is None:
            raise ValidationError("request body is required")
        try:
            length = int(length_text)
        except ValueError as exc:
            raise ValidationError("invalid Content-Length") from exc
        if length < 0 or length > 1_048_576:
            raise ValidationError("request body is too large")
        raw = self.rfile.read(length)
        return json.loads(raw.decode("utf-8"))

    def _json(self, status: HTTPStatus, body: object, write_body: bool = True) -> None:
        encoded = json.dumps(body, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode(
            "utf-8"
        )
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(encoded)))
        self.end_headers()
        if write_body:
            self.wfile.write(encoded)

    def _error(self, status: HTTPStatus, message: str, write_body: bool = True) -> None:
        self._json(status, {"error": message}, write_body=write_body)

    def log_message(self, fmt: str, *args: object) -> None:
        sys.stderr.write("outbox: " + (fmt % args) + "\n")


class OutboxHTTPServer(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, host: str, port: int, store: Store) -> None:
        super().__init__((host, port), OutboxHandler)
        self.store = store


def run_server(db: str, host: str, port: int) -> int:
    store = Store(db)
    store.initialize()
    server = OutboxHTTPServer(host, port, store)
    print(f"outbox listening on http://{host}:{port}", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        return 0
    finally:
        server.server_close()
    return 0
