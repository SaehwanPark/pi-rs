"""HTTP-agnostic request dispatch for the reading queue API.

``Api.handle`` accepts the four pieces of a request that matter (method, path,
parsed query, raw body) and returns a :class:`Response`.  Because no socket or
handler object appears here, every documented route, status code, and
validation rule can be exercised without binding a port.

Status mapping lives entirely in this module:

* :class:`~readqueue.validation.ValidationError` -> ``400``
* :class:`~readqueue.errors.UnknownItem` -> ``404``
* :class:`~readqueue.errors.DuplicateURL` -> ``409``
* method not allowed on a known path -> ``405``
* unknown path -> ``404``

JSON output goes through :func:`dumps`, which sorts object keys, so both the
keys of an item and the ordering of ``GET /items`` (ascending ``id`` from the
store) are deterministic for a given database state.
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from typing import Any, Mapping

from . import validation
from .errors import DuplicateURL, UnknownItem
from .store import ItemStore

#: ``Content-Type`` used for every response body this service produces.
JSON_CONTENT_TYPE = "application/json"


def dumps(payload: Any) -> str:
    """Serialise ``payload`` deterministically (sorted keys, stable separators)."""
    return json.dumps(payload, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


@dataclass(frozen=True)
class Response:
    """A serialisable HTTP response: status, optional JSON payload, headers."""

    status: int
    payload: Any = None
    headers: Mapping[str, str] | None = None

    @property
    def has_body(self) -> bool:
        return self.payload is not None

    def body(self) -> bytes:
        """Return the encoded body, or ``b""`` for status-only responses."""
        if self.payload is None:
            return b""
        return dumps(self.payload).encode("utf-8")

    def content_type(self) -> str:
        return (self.headers or {}).get("Content-Type", JSON_CONTENT_TYPE)


def _error(message: str, status: int = 400) -> Response:
    return Response(status, {"error": message})


_NOT_FOUND = "not found"
_METHOD_NOT_ALLOWED = "method not allowed"


class Api:
    """Maps HTTP requests onto :class:`~readqueue.store.ItemStore` calls."""

    #: Methods accepted on the collection and item routes, for 405 responses.
    COLLECTION_METHODS = ("GET", "POST")
    ITEM_METHODS = ("GET", "PATCH", "DELETE")

    def __init__(self, store: ItemStore) -> None:
        self.store = store

    # -- entry point -----------------------------------------------------

    def handle(
        self,
        method: str,
        path: str,
        query: Mapping[str, list[str]] | None = None,
        body: bytes | str = b"",
    ) -> Response:
        """Dispatch one request and return the response to send."""
        verb = method.upper()
        segments = [segment for segment in path.split("?")[0].split("/") if segment]
        params = query or {}

        if segments == ["healthz"]:
            return self._only(verb, ("GET",), self.healthz)
        if segments == ["items"]:
            return self._collection(verb, params, body)
        if len(segments) == 2 and segments[0] == "items":
            return self._item(verb, segments[1], params, body)
        return _error(_NOT_FOUND, 404)

    # -- routes ----------------------------------------------------------

    def healthz(self) -> Response:
        return Response(200, {"ok": True})

    def list_items(self, params: Mapping[str, list[str]]) -> Response:
        status = _first(params, "status")
        tag = _first(params, "tag")
        try:
            status_filter = validation.parse_status_filter(status) if status is not None else None
            tag_filter = validation.parse_tag_filter(tag) if tag is not None else None
        except validation.ValidationError as exc:
            return _error(exc.message, 400)
        return Response(200, self.store.list(status=status_filter, tag=tag_filter))

    def create_item(self, body: bytes | str) -> Response:
        document, failure = _load_json(body)
        if failure is not None:
            return failure
        try:
            values = validation.validate_create(document)
        except validation.ValidationError as exc:
            return _error(exc.message, 400)
        try:
            item = self.store.create(**values)
        except DuplicateURL as exc:
            return _error(str(exc), 409)
        return Response(201, item)

    def get_item(self, raw_id: str) -> Response:
        item_id = _resolve_id(raw_id)
        if isinstance(item_id, Response):
            return item_id
        try:
            item = self.store.get(item_id)
        except UnknownItem as exc:
            return _error(str(exc), 404)
        return Response(200, item)

    def patch_item(self, raw_id: str, body: bytes | str) -> Response:
        document, failure = _load_json(body)
        if failure is not None:
            return failure
        try:
            changes = validation.validate_patch(document)
        except validation.ValidationError as exc:
            return _error(exc.message, 400)
        item_id = _resolve_id(raw_id)
        if isinstance(item_id, Response):
            return item_id
        try:
            item = self.store.update(item_id, changes)
        except UnknownItem as exc:
            return _error(str(exc), 404)
        except DuplicateURL as exc:
            return _error(str(exc), 409)
        return Response(200, item)

    def delete_item(self, raw_id: str) -> Response:
        item_id = _resolve_id(raw_id)
        if isinstance(item_id, Response):
            return item_id
        try:
            self.store.delete(item_id)
        except UnknownItem as exc:
            return _error(str(exc), 404)
        return Response(204)

    # -- helpers ---------------------------------------------------------

    def _collection(self, verb: str, params: Mapping[str, list[str]], body: bytes | str) -> Response:
        if verb == "GET":
            return self.list_items(params)
        if verb == "POST":
            return self.create_item(body)
        return _allowed(_METHOD_NOT_ALLOWED, self.COLLECTION_METHODS)

    def _item(
        self, verb: str, raw_id: str, params: Mapping[str, list[str]], body: bytes | str
    ) -> Response:
        if verb == "GET":
            return self.get_item(raw_id)
        if verb == "PATCH":
            return self.patch_item(raw_id, body)
        if verb == "DELETE":
            return self.delete_item(raw_id)
        return _allowed(_METHOD_NOT_ALLOWED, self.ITEM_METHODS)

    @staticmethod
    def _only(verb: str, allowed: tuple[str, ...], handler) -> Response:
        if verb in allowed:
            return handler()
        return _allowed(_METHOD_NOT_ALLOWED, ", ".join(allowed))


def _allowed(message: str, methods: Any) -> Response:
    allowed = methods if isinstance(methods, str) else ", ".join(methods)
    return Response(405, {"error": message}, {"Allow": allowed})


def _first(params: Mapping[str, list[str]], name: str) -> str | None:
    """Return the first value of ``name``, or ``None`` when absent.

    Repeated parameters are tolerated and the first occurrence wins, which
    keeps behaviour predictable without inventing merge semantics the
    specification does not define.  Unknown parameters are ignored: filters are
    additive, not a contract on their absence.
    """
    values = params.get(name)
    if not values:
        return None
    return values[0]


def _resolve_id(raw_id: str) -> int | Response:
    """Parse a path id, returning a 404 response when it cannot be an id.

    A path such as ``/items/abc`` can never name a stored item, so it is
    reported as not found (``404``) rather than a body-level ``400``; the
    specification only promises ``400`` for invalid *input fields*.
    """
    try:
        return validation.parse_id(raw_id)
    except validation.ValidationError:
        return _error(_NOT_FOUND, 404)


def _load_json(body: bytes | str) -> tuple[Any, Response | None]:
    """Decode a request body, returning either the document or an error."""
    if isinstance(body, bytes):
        try:
            text = body.decode("utf-8")
        except UnicodeDecodeError:
            return None, _error("request body must be UTF-8 encoded JSON")
    else:
        text = body
    if not text.strip():
        return None, _error("request body must be a JSON object")
    try:
        return json.loads(text), None
    except json.JSONDecodeError as exc:
        return None, _error(f"malformed JSON in request body: {exc.msg}")
