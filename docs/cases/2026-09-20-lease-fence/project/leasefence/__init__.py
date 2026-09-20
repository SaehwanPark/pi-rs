"""leasefence: a dependency-free pipeline admission service with lease fencing.

The package is intentionally small.  Public surface is deliberately narrow:

* :func:`create_app` builds an HTTP request handler bound to a store and secret.
* :class:`LeaseStore` owns every durable state transition (admission, claim,
  reclaim, fenced finalization).
* :func:`run_worker` performs one bounded ``--once`` pass of direct-argv delivery.

Every module in this package imports only the Python standard library.
"""

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
