"""Command-line boundary for the HTTP service and bounded worker."""

from __future__ import annotations

import argparse

from .service import serve
from .worker import run as run_worker


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="signed pipeline intake with fenced receipt delivery")
    commands = parser.add_subparsers(dest="command")

    serve_parser = commands.add_parser("serve", help="run the signed HTTP service")
    serve_parser.add_argument("--db", required=True, help="SQLite database path")
    serve_parser.add_argument("--secret", required=True, help="HMAC secret")
    serve_parser.add_argument("--host", default="127.0.0.1")
    serve_parser.add_argument("--port", required=True, type=int)

    worker_parser = commands.add_parser("worker", help="deliver runnable jobs once")
    worker_parser.add_argument("--db", required=True, help="SQLite database path")
    worker_parser.add_argument("--sink", required=True, help="sink executable")
    worker_parser.add_argument("--sink-arg", action="append", default=[], help="one direct sink argv value")
    worker_parser.add_argument("--lease-seconds", type=int, default=30)
    worker_parser.add_argument("--once", action="store_true", help="perform one bounded pass and exit")
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    if args.command is None:
        parser.print_help()
        return 0
    if args.command == "serve":
        if not 0 < args.port <= 65535:
            parser.error("--port must be between 1 and 65535")
        serve(args.db, args.secret, args.host, args.port)
        return 0
    if not 1 <= args.lease_seconds <= 300:
        parser.error("--lease-seconds must be between 1 and 300")
    if not args.once:
        parser.error("worker requires --once")
    return run_worker(args.db, args.sink, args.sink_arg, args.lease_seconds)

