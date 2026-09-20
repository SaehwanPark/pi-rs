"""Command-line interface: argument parsing, rendering, and exit status.

The functional core lives in :mod:`tasklog.model`; this module only translates
between argv, the durable file, and the terminal.  Handlers follow one shape:
load, ask the model for a new ledger, then write.  Because the model raises
before returning a ledger, a rejected command never reaches the writer.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path
from typing import Callable, Sequence, TextIO

from . import __version__
from .model import Ledger, LedgerError, Task, parse_id
from .storage import (
    DEFAULT_FILENAME,
    ENV_PATH,
    load_ledger,
    resolve_path,
    save_ledger,
)

__all__ = ["Context", "main", "run"]

EXIT_OK = 0
#: Invalid commands, malformed ids, and unreadable state all share this code,
#: mirroring the convention that 2 means "the command as given is not usable".
EXIT_ERROR = 2
#: argparse's own code for usage problems (unknown command, missing argument).
EXIT_USAGE = 2

_Command = Callable[["Context"], int]


class Context:
    """Everything a command handler needs: options, the state file, the streams."""

    def __init__(
        self,
        args: argparse.Namespace,
        out: TextIO,
        err: TextIO,
        base: Path | None = None,
    ) -> None:
        self.args = args
        self.out = out
        self.err = err
        self.path = resolve_path(args.state, base=base)

    def load(self) -> Ledger:
        return load_ledger(self.path)

    def save(self, ledger: Ledger) -> None:
        save_ledger(self.path, ledger)

    def line(self, text: str = "") -> None:
        print(text, file=self.out)


def _add_state_option(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--state",
        metavar="PATH",
        default=None,
        help=(
            f"state file to use instead of ./{DEFAULT_FILENAME}; ${ENV_PATH} "
            "is also honoured"
        ),
    )


def build_parser() -> argparse.ArgumentParser:
    """Return the argument parser for every tasklog command."""
    parser = argparse.ArgumentParser(
        prog="python -m tasklog",
        description=(
            "tasklog - a tiny task ledger with no dependencies.\n\n"
            "Tasks live in one human-editable JSON file so the ledger can live\n"
            "in version control:\n\n"
            f"  ./{DEFAULT_FILENAME} in the current directory, unless the\n"
            f"  ${ENV_PATH} environment variable or --state PATH says otherwise.\n"
            "  The file is created on the first successful add/done/remove and\n"
            "  rewritten atomically, so a failed command never leaves a partial\n"
            "  file behind.\n\n"
            "Commands:\n"
            "  add TEXT       add an open task and print it\n"
            "  list           list open tasks in ascending id order\n"
            "  list --all     list open and completed tasks\n"
            "  done ID        mark a task complete (idempotent)\n"
            "  remove ID      delete a task; its id is never reused\n\n"
            "Ids are stable positive integers.  Completed tasks stay in the\n"
            "state file until 'remove' deletes them.\n"
        ),
        epilog=(
            "Examples:\n"
            "  python -m tasklog add write the release notes\n"
            "  python -m tasklog list --all\n"
            "  python -m tasklog done 1\n"
            "  python -m tasklog remove 2\n"
            f"  TASKLOG_PATH=/tmp/notes/{DEFAULT_FILENAME} python -m tasklog list\n"
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--version",
        action="version",
        version=f"tasklog {__version__}",
        help="show the tasklog version and exit",
    )
    subparsers = parser.add_subparsers(dest="command", metavar="<command>")

    add = subparsers.add_parser(
        "add",
        help="add an open task",
        description="Add one open task and print it. Text may contain spaces.",
    )
    _add_state_option(add)
    add.add_argument(
        "text",
        nargs="+",
        metavar="TEXT",
        help="task description; one or more words",
    )
    add.set_defaults(command="add")

    listing = subparsers.add_parser(
        "list",
        help="list tasks in ascending id order",
        description="List open tasks; use --all to include completed ones.",
    )
    _add_state_option(listing)
    listing.add_argument(
        "-a",
        "--all",
        dest="all",
        action="store_true",
        help="include completed tasks",
    )
    listing.set_defaults(command="list")

    done = subparsers.add_parser(
        "done",
        help="mark a task complete",
        description=(
            "Mark an open task complete. Repeating the command on a completed "
            "task succeeds without changing the ledger."
        ),
    )
    _add_state_option(done)
    done.add_argument("id", metavar="ID", help="id of the task to complete")
    done.set_defaults(command="done")

    remove = subparsers.add_parser(
        "remove",
        help="delete a task",
        description=(
            "Delete a task. Its id is never handed to a later task, so old "
            "references stay unambiguous."
        ),
    )
    _add_state_option(remove)
    remove.add_argument("id", metavar="ID", help="id of the task to delete")
    remove.set_defaults(command="remove")

    return parser


def _render_tasks(context: Context, tasks: Sequence[Task]) -> None:
    for task in tasks:
        context.line(task.format())


def _cmd_add(context: Context) -> int:
    ledger = context.load()
    # join before validating: argparse hands back the words of one argument.
    ledger, task = ledger.add(" ".join(context.args.text))
    context.save(ledger)
    context.line(f"Added task {task.id}: {task.text}")
    return EXIT_OK


def _cmd_list(context: Context) -> int:
    ledger = context.load()
    tasks = ledger.sorted_tasks() if context.args.all else ledger.open_tasks()
    if not tasks:
        scope = "tasks" if context.args.all else "open tasks"
        context.line(f"No {scope}. Add one with: python -m tasklog add TEXT")
        return EXIT_OK
    _render_tasks(context, tasks)
    context.line(ledger.summary())
    return EXIT_OK


def _cmd_done(context: Context) -> int:
    ledger = context.load()
    task_id = _requested_id(context)
    ledger, task, changed = ledger.mark_done(task_id)
    if not changed:
        # Nothing to persist: report the state instead of rewriting the file.
        context.line(f"Task {task.id} is already done: {task.text}")
        return EXIT_OK
    context.save(ledger)
    context.line(f"Completed task {task.id}: {task.text}")
    return EXIT_OK


def _cmd_remove(context: Context) -> int:
    ledger = context.load()
    task_id = _requested_id(context)
    ledger, task = ledger.remove(task_id)
    context.save(ledger)
    context.line(f"Removed task {task.id}: {task.text} (id {task.id} stays unused)")
    return EXIT_OK


def _requested_id(context: Context) -> int:
    """Validate a raw id string, raising a corrective error when malformed."""
    return parse_id(context.args.id)


_COMMANDS: dict[str, _Command] = {
    "add": _cmd_add,
    "list": _cmd_list,
    "done": _cmd_done,
    "remove": _cmd_remove,
}


def run(
    argv: Sequence[str] | None,
    out: TextIO,
    err: TextIO,
    base: Path | None = None,
) -> int:
    """Parse ``argv``, execute one command, and return its exit status.

    ``base`` is the directory that holds the default ``.tasklog.json``; tests
    pass a temporary directory instead of touching the process working
    directory.
    """
    parser = build_parser()
    # parse_args() on an existing parser does not re-read sys.argv.
    args = parser.parse_args(argv)
    if not getattr(args, "command", None):
        # Only global options were given: show the help instead of a bare error.
        parser.print_help(out)
        return EXIT_USAGE
    context = Context(args, out, err, base=base)
    try:
        return _COMMANDS[args.command](context)
    except LedgerError as exc:
        print(f"tasklog: {exc}", file=err)
        return EXIT_ERROR


def main(argv: Sequence[str] | None = None) -> int:
    """Entry point for ``python -m tasklog``; returns the process exit code."""
    return run(sys.argv[1:] if argv is None else argv, sys.stdout, sys.stderr)
