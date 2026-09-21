"""Pure validation and serialization rules for webhook deliveries."""

from __future__ import annotations

from dataclasses import dataclass
import json
import re
from typing import Mapping


class ValidationError(ValueError):
    """Raised when an external delivery does not match the HTTP contract."""


class DeliveryConflictError(ValueError):
    """Raised when an idempotency key is reused for different content."""


_DELIVERY_ID = re.compile(r"[A-Za-z0-9._-]+")
_FIELDS = frozenset({"delivery_id", "event_type", "payload"})


@dataclass(frozen=True)
class Delivery:
    """A durable delivery projection returned by the HTTP and worker edges."""

    delivery_id: str
    event_type: str
    payload: dict[str, object]
    status: str
    attempts: int
    last_error: str | None
    lease_expires_at: int | None

    def as_dict(self) -> dict[str, object]:
        """Return the stable JSON shape exposed by the service."""

        return {
            "delivery_id": self.delivery_id,
            "event_type": self.event_type,
            "payload": self.payload,
            "status": self.status,
            "attempts": self.attempts,
            "last_error": self.last_error,
            "lease_expires_at": self.lease_expires_at,
        }


def canonical_payload(payload: Mapping[str, object]) -> str:
    """Encode a JSON object deterministically for idempotency comparisons."""

    return json.dumps(
        dict(payload), ensure_ascii=False, sort_keys=True, separators=(",", ":")
    )


def validate_delivery(value: object) -> tuple[str, str, dict[str, object]]:
    """Validate and normalize one decoded POST body without performing I/O."""

    if not isinstance(value, dict) or set(value) != _FIELDS:
        raise ValidationError("body must contain exactly delivery_id, event_type, and payload")

    delivery_id = value["delivery_id"]
    event_type = value["event_type"]
    payload = value["payload"]
    if not isinstance(delivery_id, str) or not delivery_id.strip():
        raise ValidationError("delivery_id must be a non-empty string")
    if not _DELIVERY_ID.fullmatch(delivery_id.strip()):
        raise ValidationError("delivery_id contains unsupported characters")
    if not isinstance(event_type, str) or not event_type.strip():
        raise ValidationError("event_type must be a non-empty string")
    if type(payload) is not dict:
        raise ValidationError("payload must be a JSON object")
    return delivery_id.strip(), event_type.strip(), dict(payload)
