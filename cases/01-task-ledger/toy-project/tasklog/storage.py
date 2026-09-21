"""Durable JSON storage for the ledger.

Two rules drive this module:

* A failed command never touches the state file: validation happens on the
  decoded document, and the bytes are serialised completely before anything is
  written, so a serialisation error cannot truncate the ledger.
* A write is assembled completely before replacement.  The document goes to a
  temporary file in the same directory, is flushed to disk, then ``os.replace``
  moves it into place.  This prevents partial JSON from an ordinary interrupted
  write; it is not a claim of full power-loss durability on every filesystem.
"""

from __future__ import annotations

import json
import os
import tempfile
from pathlib import Path

from .model import CorruptState, Ledger, LedgerError

__all__ = [
    "DEFAULT_FILENAME",
    "ENV_PATH",
    "StorageError",
    "load_ledger",
    "resolve_path",
    "save_ledger",
]

DEFAULT_FILENAME = ".tasklog.json"
ENV_PATH = "TASKLOG_PATH"


class StorageError(LedgerError):
    """The state file exists but cannot be read or replaced."""


def resolve_path(
    explicit: str | os.PathLike[str] | None = None,
    base: str | os.PathLike[str] | None = None,
) -> Path:
    """Return the state file path.

    Precedence: an ``explicit`` ``--state`` argument, then the ``TASKLOG_PATH``
    environment variable (an empty value is ignored), then ``.tasklog.json``
    inside ``base``, which defaults to the current working directory.  ``base``
    exists so tests (and embedders) can point the CLI at a temporary directory
    without mutating process-global state.
    """
    if explicit is not None:
        return Path(explicit).expanduser()
    configured = os.environ.get(ENV_PATH, "").strip()
    if configured:
        return Path(configured).expanduser()
    return (Path(base) if base is not None else Path.cwd()) / DEFAULT_FILENAME


def load_ledger(path: str | os.PathLike[str]) -> Ledger:
    """Return the ledger stored at ``path``, or an empty one if it is absent.

    A missing file is a normal state (a fresh directory), not an error.  A file
    that is present but unreadable or malformed raises instead, because the
    only safe response is to stop rather than overwrite tasks we cannot parse.
    """
    path = Path(path)
    try:
        text = path.read_text(encoding="utf-8")
    except FileNotFoundError:
        return Ledger.empty()
    except IsADirectoryError:
        raise StorageError(
            f"state path {path} is a directory, not a file; point "
            f"${ENV_PATH} at a writable file or remove the directory"
        ) from None
    except OSError as exc:
        raise StorageError(
            f"cannot read state file {path}: {exc}; fix the file permissions "
            "or choose another location with ${ENV_PATH}"
        ) from None
    if not text.strip():
        raise StorageError(
            f"state file {path} is empty; repair it or delete it to start a new ledger"
        )

    try:
        document = json.loads(text)
    except ValueError as exc:
        raise StorageError(
            f"state file {path} is not valid JSON ({exc}); repair it by hand "
            "or delete it to start a new ledger"
        ) from None
    try:
        return Ledger.from_document(document)
    except CorruptState as exc:
        # Name the offending file: the user has to edit or delete it, and a
        # message without the path would leave them guessing which file.
        raise StorageError(f"{path}: {exc}") from None


def save_ledger(path: str | os.PathLike[str], ledger: Ledger) -> Path:
    """Write ``ledger`` to ``path`` atomically and return the path used.

    The parent directory must already exist: creating one would turn a typo in
    ``TASKLOG_PATH`` into a second ledger in a directory the user never chose,
    so a missing directory is reported instead.
    """
    path = Path(path)
    # Serialise first: an encoding error must not reach the state file at all.
    payload = json.dumps(ledger.as_document(), indent=2, sort_keys=True) + "\n"

    parent = path.parent if str(path.parent) else Path(".")
    try:
        if not parent.is_dir():
            raise StorageError(
                f"directory {parent} for state file {path} does not exist; "
                f"create it or point ${ENV_PATH} at a writable directory"
            )
        fd, tmp_name = tempfile.mkstemp(
            dir=str(parent), prefix=f".{path.name}.", suffix=".tmp"
        )
    except OSError as exc:
        raise StorageError(
            f"cannot prepare a temporary file next to {path}: {exc}; make the "
            "directory writable or set ${ENV_PATH} to a usable location"
        ) from None

    try:
        with os.fdopen(fd, "w", encoding="utf-8", newline="\n") as handle:
            handle.write(payload)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(tmp_name, path)
    except OSError as exc:
        _discard(tmp_name)
        raise StorageError(
            f"cannot update state file {path}: {exc}; the previous ledger is "
            "still stored there"
        ) from None
    except BaseException:
        _discard(tmp_name)
        raise
    return path


def _discard(name: str) -> None:
    try:
        os.unlink(name)
    except OSError:  # pragma: no cover - best effort cleanup
        pass
