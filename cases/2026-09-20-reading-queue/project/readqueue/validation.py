"""Pure validation of item payloads.

Nothing in this module touches the database or the network: a payload either
validates and is returned as a normalised ``dict`` ready for the store, or an
:class:`ValidationError` explains the single reason it was rejected.  Keeping
the rules here makes the HTTP status mapping in :mod:`readqueue.api` the only
place where "why" becomes "400" or "404".

Two details carry deliberate weight:

* ``title`` and ``url`` are stored exactly as submitted (the contract trims
  only tags) but a value that is empty after trimming counts as empty, so a
  body of spaces is rejected instead of quietly creating a blank item.
* Tags are normalised to trimmed, lower-case, unique, sorted values, which is
  what makes both persistence and ``GET /items?tag=...`` deterministic.
"""

from __future__ import annotations

from typing import Any, Iterable, Mapping

from .store import STATUSES

#: Fields accepted by ``POST /items``.
CREATE_FIELDS: frozenset[str] = frozenset({"title", "url", "tags"})

#: Fields accepted by ``PATCH /items/<id>`` (``status`` is patch-only).
PATCH_FIELDS: frozenset[str] = CREATE_FIELDS | {"status"}


class ValidationError(Exception):
    """Raised for any body that the documented contract rejects."""

    def __init__(self, message: str) -> None:
        super().__init__(message)
        self.message = message


def _reject(message: str) -> Any:
    raise ValidationError(message)


def _require_object(payload: Any, label: str) -> dict[str, Any]:
    if not isinstance(payload, dict):
        _reject(f"{label} must be a JSON object")
    return payload


def _require_text(value: Any, field: str, *, required: bool) -> str:
    if not isinstance(value, str):
        _reject(f"{field} must be a string")
    if not value.strip():
        _reject(f"{field} must be a non-empty string")
    return value


def normalise_tags(value: Any, field: str = "tags") -> list[str]:
    """Return ``value`` as trimmed, lower-case, unique, sorted tags."""
    if not isinstance(value, list):
        _reject(f"{field} must be a list of strings")
    normalised: set[str] = set()
    for entry in value:
        if not isinstance(entry, str):
            _reject(f"{field} must contain only strings")
        cleaned = entry.strip().lower()
        if not cleaned:
            _reject(f"{field} must contain only non-empty strings")
        normalised.add(cleaned)
    return sorted(normalised)


def _require_status(value: Any, field: str = "status") -> str:
    if not isinstance(value, str):
        _reject(f"{field} must be a string")
    if value not in STATUSES:
        options = ", ".join(STATUSES)
        _reject(f"{field} must be one of: {options}")
    return value


def _reject_unknown(payload: Mapping[str, Any], allowed: frozenset[str]) -> None:
    unknown = sorted(key for key in payload if key not in allowed)
    if unknown:
        _reject(f"unknown field(s): {', '.join(unknown)}")


def validate_create(payload: Any) -> dict[str, Any]:
    """Validate a ``POST /items`` body.

    Returns the values the store needs: ``title``, ``url`` and a normalised
    ``tags`` list.  Status is never accepted here because new items always
    start as ``queued``.
    """
    body = _require_object(payload, "request body")
    _reject_unknown(body, CREATE_FIELDS)
    for field in ("title", "url"):
        if field not in body:
            _reject(f"missing required field: {field}")
    return {
        "title": _require_text(body["title"], "title", required=True),
        "url": _require_text(body["url"], "url", required=True),
        "tags": normalise_tags(body["tags"]) if "tags" in body else [],
    }


def validate_patch(payload: Any) -> dict[str, Any]:
    """Validate a ``PATCH /items/<id>`` body into store-ready changes.

    The body must be a non-empty object; at least one of ``title``, ``url``,
    ``tags`` or ``status`` is required so a meaningless patch is refused with
    400 instead of reporting success for a no-op.
    """
    body = _require_object(payload, "request body")
    if not body:
        _reject("request body must contain at least one field")
    _reject_unknown(body, PATCH_FIELDS)
    changes: dict[str, Any] = {}
    if "title" in body:
        changes["title"] = _require_text(body["title"], "title", required=True)
    if "url" in body:
        changes["url"] = _require_text(body["url"], "url", required=True)
    if "tags" in body:
        changes["tags"] = normalise_tags(body["tags"])
    if "status" in body:
        changes["status"] = _require_status(body["status"])
    return changes


#: SQLite integers are signed 64-bit, so a longer path id can never name a row.
MAX_ID = 2**63 - 1


def parse_id(value: str) -> int:
    """Parse a path id, accepting only a plain positive integer.

    ASCII digits only (so ``١٢`` is refused), no sign, no whitespace, and no
    value beyond the SQLite integer range: each of those shapes can only ever
    mean "not a stored id", and the caller reports that as a 404.
    """
    invalid = f"id must be a positive integer, got {value!r}"
    if not value or not value.isascii() or not value.isdigit():
        raise ValidationError(invalid)
    item_id = int(value)
    if item_id <= 0 or item_id > MAX_ID:
        raise ValidationError(invalid)
    return item_id


def parse_status_filter(value: str) -> str:
    """Validate a ``?status=`` query value."""
    _require_status(value, "status")
    return value


def parse_tag_filter(value: str) -> str:
    """Validate a ``?tag=`` query value."""
    if not isinstance(value, str):
        _reject("tag must be a string")
    return value
