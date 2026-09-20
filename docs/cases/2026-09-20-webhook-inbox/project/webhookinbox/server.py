"""HTTP adapter for signed delivery admission and status reads."""

from __future__ import annotations

import hashlib
import hmac
import json
from http.server import BaseHTTPRequestHandler, HTTPServer
from typing import cast
from urllib.parse import unquote, urlsplit

from .model import DeliveryConflictError, ValidationError, validate_delivery
from .storage import Store


MAX_BODY_BYTES = 1_000_000


class WebhookInboxServer(HTTPServer):
    """HTTP server carrying the store and secret at the adapter boundary."""

    store: Store
    secret: bytes


class RequestError(ValueError):
    """Raised for a malformed HTTP request body."""


class Handler(BaseHTTPRequestHandler):
    """Serve the small JSON contract without exposing storage details."""

    protocol_version = "HTTP/1.0"

    def _server(self) -> WebhookInboxServer:
        return cast(WebhookInboxServer, self.server)

    def _send_json(self, status: int, value: object) -> None:
        body = json.dumps(value, sort_keys=True, separators=(",", ":")).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _error(self, status: int, message: str) -> None:
        self._send_json(status, {"error": message})

    def _body(self) -> bytes:
        header = self.headers.get("Content-Length")
        if header is None:
            raise RequestError("Content-Length header is required")
        try:
            length = int(header)
        except ValueError as exc:
            raise RequestError("Content-Length must be an integer") from exc
        if length < 0 or length > MAX_BODY_BYTES:
            raise RequestError("request body is too large")
        body = self.rfile.read(length)
        if len(body) != length:
            raise RequestError("request body was truncated")
        return body

    def _signed(self, body: bytes) -> bool:
        supplied = self.headers.get("X-Webhook-Signature", "")
        if not supplied.startswith("sha256=") or len(supplied) != 71:
            return False
        digest = hmac.new(self._server().secret, body, hashlib.sha256).hexdigest()
        return hmac.compare_digest(supplied, f"sha256={digest}")

    @staticmethod
    def _path_parts(path: str) -> list[str]:
        return [unquote(part) for part in urlsplit(path).path.split("/")]

    def do_GET(self) -> None:
        path = urlsplit(self.path).path
        if path == "/healthz":
            self._send_json(200, {"ok": True})
            return
        parts = self._path_parts(self.path)
        if len(parts) == 3 and parts[1] == "deliveries" and parts[2]:
            delivery = self._server().store.get(parts[2])
            if delivery is None:
                self._error(404, "delivery not found")
            else:
                self._send_json(200, delivery.as_dict())
            return
        self._error(404, "route not found")

    def do_POST(self) -> None:
        if urlsplit(self.path).path != "/deliveries":
            self._error(404, "route not found")
            return
        try:
            body = self._body()
        except RequestError as exc:
            self._error(400, str(exc))
            return
        if not self._signed(body):
            self._error(401, "invalid webhook signature")
            return
        try:
            value = json.loads(body.decode("utf-8"))
            delivery_id, event_type, payload = validate_delivery(value)
        except (UnicodeDecodeError, json.JSONDecodeError, ValidationError) as exc:
            self._error(400, str(exc))
            return
        try:
            delivery, created = self._server().store.admit(delivery_id, event_type, payload)
        except DeliveryConflictError as exc:
            self._error(409, str(exc))
            return
        self._send_json(202 if created else 200, delivery.as_dict())

    def do_PUT(self) -> None:
        self._error(405, "method not allowed")

    def do_PATCH(self) -> None:
        self._error(405, "method not allowed")

    def do_DELETE(self) -> None:
        self._error(405, "method not allowed")

    def log_message(self, format: str, *args: object) -> None:
        """Keep routine request logs out of the machine-readable oracle output."""


def make_server(host: str, port: int, database: str, secret: str) -> WebhookInboxServer:
    """Create a configured server without starting a network loop."""

    server = WebhookInboxServer((host, port), Handler)
    server.store = Store(database)
    server.secret = secret.encode("utf-8")
    return server
