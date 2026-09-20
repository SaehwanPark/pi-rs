"""Command-line entry point for the Lease Cascade service and worker."""

from __future__ import annotations

import sys

from .cli import main

if __name__ == "__main__":  # pragma: no cover - exercised through subprocess
    sys.exit(main(sys.argv[1:]))
