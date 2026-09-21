# Read Queue retry report

Date: 2026-09-20
Branch: `tester/2026-09-20-next-live-case`
Parent runtime commit: `4ae9630`
Draft PR: https://github.com/SaehwanPark/rupi/pull/108

## Scope and temporary probe

This retry used the existing branch and current `target/debug/rupi.exe`; no
new branch or PR was created. The committed `rupi.config.json` and
`rupi.recovery.config.json` were not changed. A temporary
`rupi.retry.minimal.config.json` pointed at
`http://127.0.0.1:8000/v1`, used model `qwen3.8-flash-next`, set
`thinking: "minimal"`, and bounded the turn at 12 model requests.

The current binary was rebuilt first:

```text
cargo build --bin rupi
exit 0; elapsed 205 ms
```

## Bounded live retry

The exact prompt required a read-only inspection, one Windows `dir` command,
direct `python.exe` argv for the project suite and help commands, no shell
wrapping for Python, no independent oracle, and no broad polish. The exact
CLI invocation was:

```text
rupi.exe run --config rupi.retry.minimal.config.json --cwd . --prompt "Bounded Read Queue retry verification. Work only inside this workspace and do not edit any file. Read SPEC.md and inspect the existing implementation briefly. First inspect the workspace with one exec command using dir (not Unix ls). Then, before any broad polish or other work, run the project suite with the direct process tool using program python.exe and argv [\"-W\", \"error::ResourceWarning\", \"-m\", \"unittest\", \"discover\", \"-s\", \"tests\", \"-p\", \"test_*.py\", \"-v\"]. Do not invoke Python through a shell. If the suite fails, report the exact failure and stop; do not fix it. If it passes, run the two direct-process help checks: python.exe -m readqueue --help and python.exe -m readqueue serve --help. Do not run the independent acceptance oracle; I will run that separately. Summarize exact command results, and do not claim completion from planned work."
```

Session: `01a0bf6d-7f18-768a-a68f-019fa6b52283`

```text
model requests: 7 / bounded maximum 12
turn status: completed
trace duration: 152,166 ms
observed CLI result: exit 0
tool completions: 8; failures: 0
```

The trace shows `dir`, `dir readqueue tests`, read-only inspection, then direct
process calls. There were no `ls`, fragile `cmd /c`, or failed shell calls.
The project process result was:

```text
python.exe -W error::ResourceWarning -m unittest discover -s tests -p test_*.py -v
Ran 67 tests in 5.429s
OK
exit 0
```

Both direct-process help checks exited 0. The model reported no files created,
modified, or deleted.

The successful completion against the Qwen endpoint with temporary
`thinking: "minimal"` is the live mapping probe: the old
`Unexpected reasoning effort minimal` provider failure did not recur. The
trace recorded all seven model requests as completed and no provider/tool
failure.

## Independent final verification

After the live project-suite run, the independent checks were run outside the
model session:

```text
python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v
exit 0; elapsed 5,631 ms
Ran 67 tests in 5.482s
OK

python -W error::ResourceWarning -m unittest discover -s acceptance -p "test_*.py" -v
exit 0; elapsed 1,287 ms
Ran 1 test in 1.163s
OK
```

The acceptance test covered the fresh-process HTTP contract, CRUD/filter/error
paths, and persistence after stopping and restarting against the same SQLite
database.

Trace and replay also passed:

```text
rupi.exe trace 01a0bf6d-7f18-768a-a68f-019fa6b52283 --config rupi.retry.minimal.config.json --silent
exit 0; elapsed 27 ms; 799 trace entries read

rupi.exe replay .rupi-state\sessions\01a0bf6d-7f18-768a-a68f-019fa6b52283.trace.jsonl --tools --sequence
exit 0; elapsed 30 ms
```

## Findings

- No new Read Queue project defect was found; no project code was changed.
- The minimal-to-low provider mapping works against the configured Qwen server.
- The updated Windows guidance works for this case: the model used `dir` and
  direct process/argv, with zero prior-style shell failures.
- No major runtime friction remains for this case. The only operational cost
  was the 152-second bounded live turn; it completed within the explicit
  12-request limit rather than being treated as an exhaustion.
