"""Command-line entry point for ``python -m webhookinbox``."""

from .cli import main


if __name__ == "__main__":
    raise SystemExit(main())
