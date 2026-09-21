"""Command-line boundary for the HTTP service, worker, and audit verifier."""

from __future__ import annotations

import argparse
import sys

from .audit import AuditError, tail_database, verify_database
from .service import serve
from .worker import run as run_worker


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="signed pipeline intake with fenced receipts and an audit ledger"
    )
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

    audit_parser = commands.add_parser("audit", help="verify or inspect the read-only audit ledger")
    audit_parser.add_argument("--db", required=True, help="SQLite database path")
    audit_parser.add_argument("--verify", action="store_true", help="verify every chained audit row")
    audit_parser.add_argument("--tail", type=int, help="print at most COUNT safe audit events")
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
    if args.command == "worker":
        if not 1 <= args.lease_seconds <= 300:
            parser.error("--lease-seconds must be between 1 and 300")
        if not args.once:
            parser.error("worker requires --once")
        return run_worker(args.db, args.sink, args.sink_arg, args.lease_seconds)
    if args.verify == (args.tail is not None):
        parser.error("audit requires exactly one of --verify or --tail COUNT")
    try:
        if args.verify:
            count, head = verify_database(args.db)
            suffix = f", head {head}" if head else ""
            print(f"audit ok: {count} events{suffix}")
        else:
            for event_json in tail_database(args.db, args.tail):
                print(event_json)
    except AuditError as exc:
        print(f"audit verification failed: {exc}", file=sys.stderr)
        return 1
    return 0
