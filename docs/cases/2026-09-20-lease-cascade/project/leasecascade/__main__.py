"""Command line entry point for ``python -m leasecascade``."""

from __future__ import annotations

import sys
from typing import List, Optional

from .cli import main

if __name__ == "__main__":  # pragma: no cover - exercised through subprocess
    sys.exit(main(sys.argv[1:]))


def run(args: Optional[List[str]] = None) -> int:
    """Programmatic alias for :func:`leasecascade.cli.main`."""
    return main(sys.argv[1:] if args is None else args)
