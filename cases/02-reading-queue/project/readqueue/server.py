"""``http.server`` wiring for :class:`readqueue.api.Api`.

The handler is intentionally thin: read the request, hand the pieces to the
dispatch layer, then write back whatever :class:`~readqueue.api.Response` came
out.  Keeping it thin means the HTTP surface (status codes, headers, bodies) is
decided in one place and the transport stays boring.

``ThreadingHTTPServer`` is used so a slow client cannot block the health
check.  Response bodies are always fully buffered, so ``Content-Length`` is
exact and ``204`` responses carry no body.

Requests whose body cannot be read (bad ``Content-Length``, oversized, or
chunked) answer immediately with ``400`` and raise :class:`_BodyReadError` to
skip dispatch, because the error response has already been written.
"""

from __future__ import annotations

import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, unquote, urlparse

from .api import Api, Response
from .store import ItemStore

#: Upper bound for a request body.  Far above any legitimate reading-queue
#: payload; stops a client from making the server buffer arbitrarily large
#: bodies.
MAX_BODY_BYTES = 1 << 20


class _BodyReadError(Exception):
    """Signals that an error response was already written for this request."""


class QueueRequestHandler(BaseHTTPRequestHandler):
    """Translates one HTTP exchange into an :class:`readqueue.api.Api` call."""

    server_version = "readqueue/0.1"
    # Keep-alive lets a single acceptance check drive many requests over one
    # connection; every response declares an exact Content-Length, so framing
    # stays unambiguous.
    protocol_version = "HTTP/1.1"

    # -- verbs -----------------------------------------------------------

    def do_GET(self) -> None:  # noqa: N802 - name fixed by http.server
        self._dispatch("GET")

    def do_HEAD(self) -> None:  # noqa: N802
        self._dispatch("HEAD", body=False)

    def do_POST(self) -> None:  # noqa: N802
        self._dispatch("POST")

    def do_PUT(self) -> None:  # noqa: N802
        self._dispatch("PUT")

    def do_PATCH(self) -> None:  # noqa: N802
        self._dispatch("PATCH")

    def do_DELETE(self) -> None:  # noqa: N802
        self._dispatch("DELETE")

    # -- plumbing --------------------------------------------------------

    def _dispatch(self, verb: str, include_body: bool = True) -> None:
        try:
            raw_body = self._read_body()
        except _BodyReadError:
            return
        parsed = urlparse(self.path)
        path = unquote(parsed.path)
        query = parse_qs(parsed.query, keep_blank_values=True)
        response = self._api().handle(verb, path, query, raw_body)
        self._write(response, include_body=include_body)

    def _read_body(self) -> bytes:
        raw_length = self.headers.get("Content-Length")
        try:
            length = int(raw_length) if raw_length else 0
        except ValueError:
            raise self._fail("invalid Content-Length header") from None
        if length < 0:
            raise self._fail("invalid Content-Length header")
        if length == 0:
            if "chunked" in (self.headers.get("Transfer-Encoding") or "").lower():
                raise self._fail("chunked request bodies are not supported")
            return b""
        if length > MAX_BODY_BYTES:
            raise self._fail("request body too large")
        return self.rfile.read(length)

    def _fail(self, message: str) -> _BodyReadError:
        """Write a 400 for a request we refuse to read, and mark it handled."""
        self._write(Response(400, {"error": message}))
        return _BodyReadError(message)

    def _write(self, response: Response, include_body: bool = True) -> None:
        payload = response.body() if include_body else b""
        self.send_response(response.status)
        if response.has_body:
            self.send_header("Content-Type", response.content_type())
            self.send_header("Content-Length", str(len(payload)))
        elif include_body and response.status != 204:
            self.send_header("Content-Length", "0")
        for name, value in (response.headers or {}).items():
            self.send_header(name, value)
        self.end_headers()
        if payload:
            self.wfile.write(payload)

    def _api(self) -> Api:
        """Return the API attached to the server by :func:`serve`."""
        api = getattr(self.server, "api", None)
        if api is None:
            raise RuntimeError("HTTP server has no 'api'; build it with readqueue.server.serve")
        return api

    # The default implementation logs every request, which would drown the
    # single startup line the specification asks for.
    def log_message(self, format: str, *args: object) -> None:
        return

    def log_error(self, format: str, *args: object) -> None:
        return

    def log_request(self, code: int | str = "-", size: int | str = "-") -> None:
        return


def make_server(host: str, port: int, store: ItemStore, api: Api | None = None) -> ThreadingHTTPServer:
    """Build a threaded server bound to ``host:port`` with the API attached."""

    class _QueueServer(ThreadingHTTPServer):
        daemon_threads = True
        allow_reuse_address = True

    server = _QueueServer((host, port), QueueRequestHandler)
    server.api = api if api is not None else Api(store)  # type: ignore[attr-defined]
    return server


def serve(
    db: str,
    host: str = "127.0.0.1",
    port: int = 8000,
    *,
    store: ItemStore | None = None,
) -> None:
    """Open or create ``db``, bind ``host:port``, and serve until interrupted.

    ``port=0`` is accepted for tests: the OS picks a free port and the printed
    startup line reports the address actually bound.
    """
    item_store = store if store is not None else ItemStore(db)
    try:
        server = make_server(host, port, item_store)
    except OSError as exc:
        item_store.close()
        raise SystemExit(f"cannot bind {host}:{port}: {exc}") from exc
    bound_host, bound_port = server.server_address[0], server.server_address[1]
    print(
        f"readqueue serving http://{bound_host}:{bound_port} (db: {item_store.path})",
        flush=True,
    )
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("readqueue stopping", flush=True)
    finally:
        server.shutdown()
        server.server_close()
        item_store.close()
