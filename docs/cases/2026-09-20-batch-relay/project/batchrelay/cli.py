from __future__ import annotations

import argparse

from .server import serve
from .worker import run_worker


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="signed dependency-batch relay")
    subparsers = parser.add_subparsers(dest="command")

    serve_parser = subparsers.add_parser("serve", help="run the HTTP batch service")
    serve_parser.add_argument("--db", required=True, help="SQLite database path")
    serve_parser.add_argument("--secret", required=True, help="HMAC signing secret")
    serve_parser.add_argument("--host", default="127.0.0.1")
    serve_parser.add_argument("--port", type=int, required=True)

    worker_parser = subparsers.add_parser("worker", help="deliver runnable jobs to a sink")
    worker_parser.add_argument("--db", required=True, help="SQLite database path")
    worker_parser.add_argument("--sink", required=True, help="sink executable")
    worker_parser.add_argument("--sink-arg", action="append", default=[], help="direct sink argument")
    worker_parser.add_argument("--lease-seconds", type=int, default=30)
    worker_parser.add_argument("--once", action="store_true", required=True, help="process the current runnable work")
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    args = parser.parse_args(argv)
    if args.command is None:
        parser.print_help()
        return 0
    try:
        if args.command == "serve":
            return serve(args.db, args.secret, args.host, args.port)
        return run_worker(args.db, args.sink, args.sink_arg, args.lease_seconds)
    except (OSError, ValueError) as exc:
        parser.error(str(exc))
    return 2
