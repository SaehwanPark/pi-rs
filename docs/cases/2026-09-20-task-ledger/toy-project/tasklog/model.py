"""Pure task-ledger model: no I/O, no mutation of an existing ledger.

Every operation returns a *new* ledger, so a command that fails validation
never has to reason about half-applied state: the caller keeps holding the
ledger it loaded until the operation succeeds.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Iterable, Sequence

STATE_VERSION = 1

__all__ = [
    "CorruptState",
    "InvalidTaskText",
    "Ledger",
    "LedgerError",
    "STATE_VERSION",
    "Task",
    "TaskNotFound",
    "parse_id",
]


class LedgerError(Exception):
    """Base class for every user-facing tasklog failure."""


class TaskNotFound(LedgerError):
    """No task exists with the requested id."""

    def __init__(self, task_id: int) -> None:
        super().__init__(
            f"no task with id {task_id}; run 'python -m tasklog list --all' "
            "to see the ids that exist"
        )
        self.task_id = task_id


class InvalidTaskText(LedgerError):
    """The supplied task text is unusable."""


class CorruptState(LedgerError):
    """The stored document cannot be understood, so it must not be replaced."""


def parse_id(value: object) -> int:
    """Return ``value`` as a positive int id, or raise a corrective error.

    Accepts ints and int-like strings because ids arrive both from the CLI
    (always ``str``) and from the JSON document (always ``int``).  Booleans and
    float-ish strings such as ``"1.5"`` are rejected: they signal a typo, not
    an id the user can act on.
    """
    if isinstance(value, bool) or value is None:
        raise _bad_id(value)
    if isinstance(value, int):
        task_id = value
    elif isinstance(value, str):
        text = value.strip()
        if not text.lstrip("+-").isdigit():
            raise _bad_id(value)
        try:
            task_id = int(text)
        except ValueError:  # pragma: no cover - guarded by isdigit()
            raise _bad_id(value) from None
    else:
        raise _bad_id(value)
    if task_id <= 0:
        raise _bad_id(value)
    return task_id


def _bad_id(value: object) -> LedgerError:
    return LedgerError(
        f"invalid id {value!r}: expected a positive whole number such as 1 "
        f"(see 'python -m tasklog list')"
    )


def _clean_text(value: object) -> str:
    if not isinstance(value, str):
        raise InvalidTaskText(
            f"invalid task text {value!r}: expected plain text"
        )
    text = " ".join(value.split())
    if not text:
        raise InvalidTaskText(
            "task text must not be empty; give a short description, for "
            "example: python -m tasklog add write tests"
        )
    return text


@dataclass(frozen=True)
class Task:
    """A single ledger entry."""

    id: int
    text: str
    done: bool = False

    @property
    def status(self) -> str:
        return "done" if self.done else "open"

    @property
    def mark(self) -> str:
        """Status column used by the CLI listings."""
        return "[x]" if self.done else "[ ]"

    def as_marked(self, done: bool) -> "Task":
        return Task(id=self.id, text=self.text, done=bool(done))

    def as_document(self) -> dict:
        return {"id": self.id, "text": self.text, "done": self.done}

    def format(self) -> str:
        return f"{self.id:>4} {self.mark} {self.text}"


@dataclass(frozen=True)
class Ledger:
    """Immutable collection of tasks plus the next id to hand out.

    ``next_id`` is the reason ids stay stable after ``remove``: the counter is
    part of the durable state and never falls back, so a deleted id can never
    be reused by an unrelated new task.
    """

    next_id: int = 1
    tasks: tuple[Task, ...] = ()

    # -- queries ---------------------------------------------------------

    def __post_init__(self) -> None:
        object.__setattr__(self, "tasks", tuple(self.tasks))
        object.__setattr__(self, "next_id", parse_id(self.next_id))
        _check_unique_ids(self.tasks)

    @classmethod
    def empty(cls) -> "Ledger":
        return cls()

    def get(self, task_id: int) -> Task:
        for task in self.tasks:
            if task.id == task_id:
                return task
        raise TaskNotFound(task_id)

    def find(self, task_id: int) -> Task | None:
        try:
            return self.get(task_id)
        except TaskNotFound:
            return None

    def open_tasks(self) -> tuple[Task, ...]:
        return tuple(task for task in self.sorted_tasks() if not task.done)

    def sorted_tasks(self) -> tuple[Task, ...]:
        return tuple(sorted(self.tasks, key=lambda task: task.id))

    def counts(self) -> tuple[int, int]:
        """Return ``(open, done)`` for the ledger in its stored order."""
        open_count = sum(1 for task in self.tasks if not task.done)
        return open_count, len(self.tasks) - open_count

    def summary(self) -> str:
        """Return ``"2 open, 1 done"``, omitting empty buckets."""
        open_count, done_count = self.counts()
        parts = []
        if open_count:
            parts.append(f"{open_count} open")
        if done_count:
            parts.append(f"{done_count} done")
        return ", ".join(parts) if parts else "empty"

    # -- commands (each returns a new ledger) ----------------------------

    def add(self, text: object) -> tuple["Ledger", Task]:
        """Append a new open task; returns the new ledger and the task."""
        task = Task(id=parse_id(self.next_id), text=_clean_text(text))
        ledger = Ledger(next_id=task.id + 1, tasks=self.tasks + (task,))
        return ledger, task

    def mark_done(self, task_id: int) -> tuple["Ledger", Task, bool]:
        """Mark ``task_id`` complete.

        Idempotent: completing an already-completed task returns the ledger
        unchanged with ``changed=False``.  Raises :class:`TaskNotFound` for an
        unknown id, leaving the original ledger untouched.
        """
        task_id = parse_id(task_id)
        task = self.get(task_id)
        if task.done:
            return self, task, False
        tasks = tuple(
            t.as_marked(True) if t.id == task_id else t for t in self.tasks
        )
        return Ledger(next_id=self.next_id, tasks=tasks), task, True

    def remove(self, task_id: int) -> tuple["Ledger", Task]:
        """Delete ``task_id``, keeping ``next_id`` so the id is never reused."""
        task_id = parse_id(task_id)
        task = self.get(task_id)
        tasks = tuple(t for t in self.tasks if t.id != task_id)
        return Ledger(next_id=self.next_id, tasks=tasks), task

    # -- document conversion --------------------------------------------

    def as_document(self) -> dict:
        """Return the JSON-ready document with deterministic ordering."""
        return {
            "next_id": self.next_id,
            "tasks": [task.as_document() for task in self.sorted_tasks()],
            "version": STATE_VERSION,
        }

    @classmethod
    def from_document(cls, document: object) -> "Ledger":
        """Rebuild a ledger from a decoded document.

        Deliberately strict: an unrecognised or inconsistent document raises
        :class:`CorruptState` instead of being repaired by dropping the tasks
        we could not read, which is how a valid task would get lost.
        """
        if not isinstance(document, dict):
            raise CorruptState(
                "state file must contain a JSON object; inspect the file and "
                "repair or delete it"
            )
        unexpected = sorted(set(document) - {"version", "next_id", "tasks"})
        if unexpected:
            raise CorruptState(
                f"state file has unknown key(s) {', '.join(unexpected)}; "
                f"expected only 'version', 'next_id' and 'tasks'"
            )

        version = document.get("version")
        if isinstance(version, bool) or version != STATE_VERSION:
            raise CorruptState(
                f"state file has unsupported version {version!r}; this build "
                f"of tasklog reads version {STATE_VERSION}"
            )

        raw_tasks = document.get("tasks")
        if not isinstance(raw_tasks, list):
            raise CorruptState(
                "state file is missing its 'tasks' list; inspect the file and "
                "repair or delete it"
            )
        tasks = tuple(cls._task_from_document(raw) for raw in raw_tasks)
        _check_unique_ids(tasks)

        next_id = document.get("next_id")
        if next_id is None:
            # Tolerate a hand-edited file that dropped the counter: ids stay
            # stable because it resumes above the highest id we can see.
            next_id = max((task.id for task in tasks), default=0) + 1
        try:
            next_id = parse_id(next_id)
        except LedgerError as exc:
            # Every document-validation failure must be a CorruptState so the
            # storage layer prefixes the offending file path for the user.
            raise CorruptState(f"state file: {exc}") from None

        highest = max((task.id for task in tasks), default=0)
        if next_id <= highest:
            raise CorruptState(
                f"state file is inconsistent: next_id is {next_id} but it "
                f"must be greater than every task id (highest is {highest}); "
                f"raise next_id or remove the conflicting task"
            )
        return cls(next_id=next_id, tasks=tasks)

    @staticmethod
    def _task_from_document(raw: object) -> Task:
        if not isinstance(raw, dict):
            raise CorruptState(
                f"state file task entry {raw!r} is not an object; inspect the "
                f"file and repair or delete it"
            )
        unexpected = sorted(set(raw) - {"id", "text", "done"})
        if unexpected:
            raise CorruptState(
                f"state file task entry has unknown key(s) "
                f"{', '.join(unexpected)}; expected only 'id', 'text', 'done'"
            )
        try:
            task = Task(
                id=parse_id(raw.get("id")),
                text=_clean_text(raw.get("text")),
                # A hand-written task without 'done' is a new open task, so an
                # absent flag means open rather than a corrupt document.
                done=_flag(raw["done"]) if "done" in raw else False,
            )
        except LedgerError as exc:
            raise CorruptState(f"state file: {exc}") from None
        return task


def _flag(value: object) -> bool:
    if isinstance(value, bool):
        return value
    raise CorruptState(
        f"state file flag {value!r} is not true or false; inspect the file "
        f"and repair or delete it"
    )


def _check_unique_ids(tasks: Iterable[Task] | Sequence[Task]) -> None:
    seen: set[int] = set()
    for task in tasks:
        if task.id in seen:
            raise CorruptState(
                f"state file lists task id {task.id} more than once; inspect "
                f"the file and repair or delete it"
            )
        seen.add(task.id)



