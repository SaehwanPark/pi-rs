"""Entry point so ``python -m readqueue`` starts :func:`readqueue.cli.main`."""

from __future__ import annotations

from .cli import main

if __name__ == "__main__":
    raise SystemExit(main())
