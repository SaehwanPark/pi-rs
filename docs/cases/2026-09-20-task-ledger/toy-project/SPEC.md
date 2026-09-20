# Task ledger toy-project specification

## Goal

Build `tasklog`, a dependency-free Python 3 command-line task ledger. It should
be useful as a tiny real project rather than a one-file demo: commands, durable
state, validation, and automated tests should be present, readable, and easy to
run from a clean checkout.

## User-facing behavior

The entry point is:

```text
python -m tasklog <command> [options]
```

State is stored in `.tasklog.json` in the current working directory unless the
program exposes a documented path override. The file is created on the first
successful write. JSON should be human-readable and deterministic enough for a
user to inspect in version control.

Required commands:

1. `add TEXT`
   - create one open task;
   - assign a stable positive integer id;
   - print the created task in a concise, human-readable form.
2. `list`
   - show open tasks in ascending id order;
   - `list --all` shows both open and completed tasks;
   - an empty ledger succeeds and says that there are no matching tasks.
3. `done ID`
   - mark an existing open task complete;
   - be idempotent for an already-completed task or report that state clearly;
   - reject a missing or malformed id without changing the ledger.
4. `remove ID`
   - delete an existing task;
   - reject a missing or malformed id without changing the ledger.

The CLI must also provide `--help` text and non-zero exit status for malformed
commands or missing required arguments. Error messages go to stderr and should
identify the corrective action where practical.

## Data and safety requirements

- Use only the Python standard library.
- Do not silently discard a valid existing task when an input is invalid.
- Keep task ids stable after removal; the next added task must not reuse an id
  that could make an old reference ambiguous.
- Keep completed tasks in the state file until explicitly removed.
- A failed command must not leave a partially written JSON document.

## Evaluation

The project succeeds when all of the following are true:

- `python -m unittest discover -s tests -v` passes from the project root.
- A clean smoke sequence can add two tasks, list them, complete one, list with
  `--all`, remove the other, and observe the expected durable state across
  separate Python processes.
- Invalid ids and missing arguments return non-zero status, write an actionable
  error to stderr, and leave `.tasklog.json` byte-for-byte unchanged.
- A fresh directory can run `python -m tasklog list` without crashing or
  requiring a pre-created state file.
- `python -m tasklog --help` documents the available commands and persistence
  behavior.

## Deliberate non-goals

- due dates, priorities, tags, dependencies, or recurring tasks;
- multiple users, locking, synchronization, or a server;
- third-party packages;
- migrations from another task manager;
- a polished TUI.

