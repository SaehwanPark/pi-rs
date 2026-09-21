"""Argument parsing and process-level composition for Webhook Inbox."""

from __future__ import annotations

import argparse
from typing import Sequence

from .server import make_server
from .worker import run_worker


def _lease_seconds(value: str) -> int:
    try:
        parsed = int(value)
    except ValueError as exc:
        raise argparse.ArgumentTypeError("lease seconds must be an integer") from exc
    if not 1 <= parsed <= 300:
        raise argparse.ArgumentTypeError("lease seconds must be between 1 and 300")
    return parsed


def build_parser() -> argparse.ArgumentParser:
    """Build the CLI parser used by both help output and execution."""

    parser = argparse.ArgumentParser(
        prog="python -m webhookinbox",
        description="Accept signed webhook deliveries and deliver them with a bounded worker.",
    )
    commands = parser.add_subparsers(dest="command", required=True)
    serve = commands.add_parser("serve", help="run the signed HTTP service")
    serve.add_argument("--db", required=True, help="SQLite database path")
    serve.add_argument("--secret", required=True, help="shared HMAC secret")
    serve.add_argument("--host", default="127.0.0.1")
    serve.add_argument("--port", type=int, default=8080)
    worker = commands.add_parser("worker", help="deliver available rows to a sink")
    worker.add_argument("--db", required=True, help="SQLite database path")
    worker.add_argument("--sink", required=True, help="sink executable")
    worker.add_argument("--sink-arg", action="append", default=[], help="direct sink argument")
    worker.add_argument("--lease-seconds", type=_lease_seconds, default=30)
    worker.add_argument("--once", action="store_true", help="required bounded drain mode")
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    """Run the requested service or bounded worker."""

    parser = build_parser()
    args = parser.parse_args(argv)
    if args.command == "serve":
        if not args.secret:
            parser.error("--secret must not be empty")
        server = make_server(args.host, args.port, args.db, args.secret)
        print(f"webhookinbox listening on http://{args.host}:{args.port}", flush=True)
        try:
            server.serve_forever(poll_interval=0.2)
        except KeyboardInterrupt:
            return 0
        finally:
            server.server_close()
        return 0
    if not args.once:
        parser.error("worker requires --once")
    return run_worker(args.db, args.sink, args.sink_arg, args.lease_seconds)
