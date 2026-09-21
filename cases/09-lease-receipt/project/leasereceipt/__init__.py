"""A dependency-free signed pipeline service with fenced, receipt-aware delivery."""

from __future__ import annotations

__all__ = [
    "STATUS_BLOCKED",
    "STATUS_FAILED",
    "STATUS_LEASED",
    "STATUS_PENDING",
    "STATUS_SUCCEEDED",
    "__version__",
]

__version__ = "0.1.0"

STATUS_PENDING = "pending"
STATUS_LEASED = "leased"
STATUS_SUCCEEDED = "succeeded"
STATUS_FAILED = "failed"
STATUS_BLOCKED = "blocked"
