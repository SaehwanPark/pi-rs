from __future__ import annotations

import json
import re
from typing import Any, Mapping


EVENT_KEYS = frozenset({"event_id", "topic", "payload"})
EVENT_ID = re.compile(r"^[A-Za-z0-9._-]+$")


class ValidationError(ValueError):
    """An input document does not satisfy the public event contract."""


def normalize_event(document: object) -> dict[str, Any]:
    if not isinstance(document, Mapping):
        raise ValidationError("request body must be a JSON object")
    if set(document) != EVENT_KEYS:
        raise ValidationError("request must contain only event_id, topic, and payload")

    event_id = document["event_id"]
    topic = document["topic"]
    payload = document["payload"]
    if not isinstance(event_id, str) or not event_id.strip():
        raise ValidationError("event_id must be a non-empty string")
    event_id = event_id.strip()
    if not EVENT_ID.fullmatch(event_id):
        raise ValidationError("event_id may contain only ASCII letters, digits, '.', '_' or '-'")
    if not isinstance(topic, str) or not topic.strip():
        raise ValidationError("topic must be a non-empty string")
    if not isinstance(payload, dict):
        raise ValidationError("payload must be a JSON object")
    return {"event_id": event_id, "topic": topic.strip(), "payload": payload}


def encode_payload(payload: dict[str, Any]) -> str:
    return json.dumps(payload, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def event_from_row(row: Mapping[str, Any]) -> dict[str, Any]:
    return {
        "event_id": row["event_id"],
        "topic": row["topic"],
        "payload": json.loads(row["payload"]),
        "status": row["status"],
        "attempts": int(row["attempts"]),
        "last_error": row["last_error"],
    }
