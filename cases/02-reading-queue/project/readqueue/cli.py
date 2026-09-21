"""Command line interface: ``python -m readqueue``.

Only one subcommand exists (``serve``) because the specification deliberately
excludes a CLI client, migrations, and multiple database backends.  Argument
parsing lives here so :mod:`readqueue.server` stays free of argparse and can be
driven directly by tests.
"""

from __future__ import annotations

import argparse
from typing import Sequence

from .server import serve
from .store import DEFAULT_DB_PATH

_DESCRIPTION = "Read Queue: a dependency-free HTTP JSON service for a reading queue."
_EPILOG = (
    "examples:\n"
    "  python -m readqueue serve --db ./var/readqueue.sqlite3 --host 127.0.0.1 --port 8000\n"
    "\n"
    "routes:\n"
    "  GET    /healthz          liveness check\n"
    "  GET    /items            list items (?status=queued|reading|done&tag=TAG)\n"
    "  POST   /items            create {title, url, tags?}\n"
    "  GET    /items/<id>       read one item\n"
    "  PATCH  /items/<id>       update {title?, url?, tags?, status?}\n"
    "  DELETE /items/<id>       remove one item"
)


def build_parser() -> argparse.ArgumentParser:
    """Return the argument parser for ``python -m readqueue``."""
    parser = argparse.ArgumentParser(
        prog="python -m readqueue",
        description=_DESCRIPTION,
        epilog=_EPILOG,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    subcommands = parser.add_subparsers(dest="command", metavar="COMMAND")
    serve_parser = subcommands.add_parser(
        "serve",
        help="start the HTTP JSON service",
        description="Start the Read Queue HTTP JSON service.",
        epilog=_EPILOG,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    serve_parser.add_argument(
        "--db",
        default=DEFAULT_DB_PATH,
        metavar="PATH",
        help="SQLite database file to use (created, with its parent directory, "
        f"when missing; default: {DEFAULT_DB_PATH})",
    )
    serve_parser.add_argument(
        "--host",
        default="127.0.0.1",
        metavar="HOST",
        help="address to bind (default: 127.0.0.1)",
    )
    serve_parser.add_argument(
        "--port",
        type=int,
        default=8000,
        metavar="PORT",
        help="TCP port to bind; 0 asks the OS for a free port (default: 8000)",
    )
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    """Parse ``argv`` and run the requested command, returning an exit code."""
    parser = build_parser()
    args = parser.parse_args(argv)
    if args.command is None:
        parser.print_help()
        return 2
    if args.command == "serve":
        if not 0 <= args.port <= 65535:
            parser.error("--port must be between 0 and 65535")
        serve(db=args.db, host=args.host, port=args.port)
        return 0
    parser.error(f"unknown command: {args.command}")  # pragma: no cover
    return 2


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
