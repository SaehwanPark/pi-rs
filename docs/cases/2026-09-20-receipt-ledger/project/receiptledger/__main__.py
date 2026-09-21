"""Entry point for ``python -m receiptledger``."""

from __future__ import annotations

import sys

from .cli import main

if __name__ == "__main__":  # pragma: no cover - exercised through subprocess.
    sys.exit(main(sys.argv[1:]))
