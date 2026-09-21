from __future__ import annotations

import argparse
import sqlite3
import sys

from .server import run_server
from .worker import run_worker


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Durable HTTP event outbox")
    commands = parser.add_subparsers(dest="command", required=True)

    serve = commands.add_parser("serve", help="run the HTTP event service")
    serve.add_argument("--db", required=True, help="SQLite database path")
    serve.add_argument("--host", required=True, help="bind address")
    serve.add_argument("--port", required=True, type=int, help="bind port")

    worker = commands.add_parser("worker", help="deliver pending events to a line-protocol sink")
    worker.add_argument("--db", required=True, help="SQLite database path")
    worker.add_argument("--sink", required=True, help="sink executable")
    worker.add_argument("--sink-arg", action="append", default=[], help="one direct sink argv value")
    worker.add_argument("--once", action="store_true", required=True, help="process a bounded snapshot")
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        if args.command == "serve":
            return run_server(args.db, args.host, args.port)
        return run_worker(args.db, args.sink, args.sink_arg)
    except (OSError, sqlite3.Error, ValueError) as exc:
        print(f"outbox: {exc}", file=sys.stderr)
        return 2
