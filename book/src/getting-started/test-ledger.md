# Live example: Test Ledger

The repository includes a small real project called **Test Ledger**. It is a
dependency-free Python 3 command-line task ledger with durable JSON state and a
test suite. It is small enough to understand in one sitting, but it exercises the
workflow that matters: inspect a specification, make a bounded change, run tests,
and inspect what rupi recorded.

The source lives at
[`docs/cases/2026-09-20-task-ledger/toy-project`](https://github.com/SaehwanPark/rupi/tree/main/docs/cases/2026-09-20-task-ledger/toy-project).
The package is named `tasklog`, so the Python commands below use `python -m tasklog`.

## 1. Prepare a disposable copy

Clone rupi if you have not already:

```bash
git clone https://github.com/SaehwanPark/rupi.git
cd rupi/docs/cases/2026-09-20-task-ledger/toy-project
```

The example requires Python 3 and no third-party packages. If you plan to let the
model edit files, work in a copy or a version-controlled branch. The checked-in
example config enables mutations because it was used for the original case study;
set `tools.auto_approve_mutating` to `false` before your first read-only request.

## 2. Verify the project without a model

Run the project’s own tests and a small command sequence first:

```bash
python -m unittest discover -s tests -p "test_*.py" -v
python -m tasklog add "write the release notes"
python -m tasklog add "run the tests"
python -m tasklog list
python -m tasklog done 1
python -m tasklog list --all
```

The independent acceptance oracle starts a fresh Python process for every command:

```bash
cd ..
python -m unittest discover -s acceptance -p "test_tasklog_subprocess.py" -v
cd toy-project
```

On Windows PowerShell, the same Python commands work. Use `Set-Location` or `cd`
to change directories; do not paste Bash line-continuation backslashes into a
PowerShell command.

## 3. Ask rupi for a read-only review

Start the local model described in the [quickstart](quickstart.md), then ask for
one narrow, read-only inspection:

```bash
rupi run --config rupi.config.json --cwd . \
  --prompt "Read SPEC.md and the project files. Do not change anything. Summarize the contract, the state-file safety rules, and the tests that prove them."
```

The assistant answer is on `stdout`; the execution transcript and diagnostics are
on `stderr`. This separation lets you save the answer without losing the evidence
about what the agent did.

For a controlled edit, make a backup or commit first, set
`auto_approve_mutating` to `true` only in this disposable workspace, and ask for one
small change followed by the test command. Keep the request bounded; a first task
does not need to redesign the whole project.

## 4. Inspect and replay the evidence

The run creates `.rupi-state/` in the example directory. Find the session id in
the run output, then inspect it without starting the provider or executing old
commands:

```bash
rupi trace --config rupi.config.json --quiet --no-reasoning
rupi replay .rupi-state/sessions/<session-id>.trace.jsonl --tools --sequence
```

Replay is read-only. If a turn is interrupted, use the session id with
`--resume` after inspecting the trace and make the next prompt a small completion
slice. A partial workspace is not the same thing as a verified project; run the
Python suite independently before calling the work complete.

## What this example demonstrates

- a clear specification and a bounded workspace;
- safe read-only inspection before enabling mutations;
- durable state that survives separate Python processes;
- independent verification rather than trusting the model’s final prose;
- trace and replay as recovery and audit tools.

The example is deliberately not a framework tutorial. It is a concrete first
project for learning the rupi workflow.
