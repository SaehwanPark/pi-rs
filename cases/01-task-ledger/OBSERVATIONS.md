# Observations log

This is the chronological evidence log for the first-user test. The final
classification and recommendations are in `FINAL_REPORT.md`.

## Starting conditions

- Repository branch: `test/first-user-task-ledger`
- Existing unrelated worktree item: `.pi/.goals-pool-snapshot.json` (left untouched)
- Endpoint: `http://127.0.0.1:8000/v1`
- Model alias: `qwen3.8-flash-next`
- Harness source invocation: `cargo run --quiet -- ...` from the `rupi` checkout
- Mutation policy for the isolated toy workspace: enabled in
  `toy-project/rupi.config.json`

## Observation entry template

For each interaction, record:

```text
### <id> — <short title>
Condition:
Command/prompt:
Expected:
Observed:
Exit/status:
Evidence:
Classification:
Impact:
```

## Live log

The entries below will be appended as the case runs. No `rupi` issue is being
fixed during this session.

### F-01 — First documented coding turn exhausted its request budget

Condition: Fresh toy workspace containing only `SPEC.md`, `rupi.config.json`, and
`.gitignore`; local Qwen endpoint reachable; mutations enabled only for this
isolated workspace.

Command/prompt: `target/debug/rupi.exe run --config rupi.config.json --cwd .`
with the implementation prompt recorded in the session trace.

Expected: one durable coding turn that implements the project, runs its tests,
and returns a final summary.

Observed: the turn ran for 1,480,843 ms (about 24m 41s), created `tasklog/`
model, storage, and CLI modules, and ran several smoke commands, but never
created tests or a README and never returned a final answer. The runtime stopped
after 32 model requests with the diagnostic `turn stopped after 32 model
requests without a final answer`, then ended the session with
`turn aborted: model request budget exhausted`. The process exit code was 1.

Exit/status: `1`; trace contains 9,917 entries and is about 4.38 MiB.

Evidence: session `01a0bd1d-3e4a-72fd-812b-e67ed5aa2494`; the terminal event is
`turn_completed` with `status: budget_exhausted`, followed by `session_ended`
with a fatal reason. `rupi trace` and `rupi replay --tools --sequence` both
read the partial session successfully.

Classification: high UX failure / possible product-policy bug. Slow local
generation is expected on this machine, but the documented first-session path
offers no request-budget explanation, progress estimate, or recovery guidance;
the first coding task fails before its acceptance checks are created.

Impact: a new user cannot tell whether the model is still working, whether the
turn is recoverable, or how to continue without losing the partial work.

### F-02 — Windows shell mismatch and quoting friction

Condition: Same first turn on Windows, where the built-in `exec` tool launches a
`cmd.exe`-style environment.

Expected: the model should be able to use ordinary workspace inspection and
Python verification commands.

Observed: the first generated probe `ls -la && ...` failed because `ls` was not
recognized. The model adapted to `dir`, but a normal probe of
`python -c "import sys; print(...)"` repeatedly became a Python
`SyntaxError: unterminated string literal`. Later attempts to use `cmd` error
levels and inline Python hashing also failed or reported misleading `exit=0`
because `%ERRORLEVEL%` was expanded in a compound command. The model spent
multiple requests working around the shell instead of testing the project.

Exit/status: individual tool failures were recorded as `ToolFailed`; the
overall turn eventually exhausted its model-request budget.

Evidence: trace tool calls at sequences 20/22, 44/46, 8835/8837, 8870/8872,
9515/9517, and 9666/9668. The transcript explicitly says `ls` is not
recognized and shows the Python quoting failures.

Classification: medium UX friction, with a possible Windows `exec` quoting
bug. The manual documents Unix `sh -c` behavior but does not state the Windows
shell or provide Windows-safe quoting examples; the model was not given a
reliable way to inspect the actual shell.

Impact: routine commands fail, error output is noisy, and exit-code validation
becomes difficult precisely when the agent is trying to verify safety behavior.

### O-03 — Workspace confinement rejected an attempted escape

Condition: The model tried to write a temporary batch file using a path with
parent traversal and an unexpanded `%USERNAME%` segment while trying to work
around the shell.

Expected: a write outside `--cwd` must be refused.

Observed: `rupi` refused the write and reported the resolved path and the
workspace-root violation. No file was created outside the toy workspace.

Exit/status: the tool became `Failed`; the session continued until the later
request-budget abort.

Evidence: trace sequences 9913–9914; the failure names the attempted path and
resolved `C:\Users\saehwan\repos\pi-rs\%USERNAME%\...` path.

Classification: positive safety observation. The error was specific enough to
explain the refusal, although it consumed the final model request.

### O-04 — Trace and replay remain usable after an aborted turn

Condition: Session ended with a fatal budget status and a partial implementation.

Expected: inspection commands should not restart the provider or execute old
tools.

Observed: `rupi trace --config rupi.config.json <session-id>` rendered the
chronological transcript and showed request durations, tool states, reductions,
and the final budget diagnostic. `rupi replay <trace> --tools --sequence`
returned a concise ordered tool lifecycle projection and did not contact
llama.cpp or rerun tools.

Classification: positive observability result. The `trace` default is very
verbose for a 9,917-event session; `--no-reasoning` or `--quiet` is needed for
practical inspection.

### F-05 — Resume accepts the partial session but does not reach completion

Condition: The session from F-01 was reopened with the documented `--resume`
flag, using a second case-only config with the same endpoint and
`thinking: "low"`. The original quickstart config was left unchanged.

Command/prompt: `rupi run --config rupi.recovery.config.json --cwd .
--resume 01a0bd1d-3e4a-72fd-812b-e67ed5aa2494 --prompt ...` asking only for
tests, README, and a short verification run.

Expected: resume the partial work, finish the missing deliverables, and return a
final summary.

Observed: the session reopened and appended new events to the same trace. The
first resumed request took 124 seconds and its first visible delta arrived at
115 seconds. Over the next roughly 25 minutes it added `tests/support.py`,
`tests/test_model.py`, `tests/test_storage.py`, and `tests/test_cli.py`, but did
not create `README.md` or return a final answer. The client was stopped after a
bounded additional wait while the model was still generating; the wrapper
reported exit `-1` and only a failed tool diagnostic.

Evidence: the resumed trace reaches sequences above 20,000 and grows to about
9 MiB; model request records show resumed requests lasting 123,180 ms,
300,311 ms, and longer. The generated files remain in the toy workspace.

Classification: high recovery UX failure. Resume is durable and productive,
but the documented recovery path still requires another long opaque model turn
and gives no user-facing estimate or way to request a bounded continuation.

Impact: a new user can recover partial files but cannot predict whether the
session will finish, and stopping the client leaves the recovery attempt without
a clean final transcript.

### F-06 — Independently validated toy project is only partially complete

Condition: After the two model turns, run the project checks directly from the
toy-project root with Python 3.14.7.

Expected: the acceptance command `python -m unittest discover -s tests -v`
passes, followed by the smoke and invalid-input checks in `SPEC.md`.

Observed: the suite discovered 78 tests but exited 1 with 3 failures and 11
errors. The generated tests unpack `Ledger.mark_done()` as two values while the
implementation returns three, so several model/storage tests error before their
assertions. Other failures include acceptance of an Arabic-Indic digit as an id,
the default-list summary including completed-task counts, and a `--state` option
test placing the option before the subcommand even though the parser only accepts
it after the subcommand. No README was generated.

The independent smoke sequence did pass: fresh `list`, two `add` calls,
`list`, `done`, `list --all`, `remove`, durable JSON inspection, unknown-id
rejection with an unchanged SHA-256, missing-argument rejection, and `--help`
all returned the expected success/non-zero statuses for the tested cases.

Classification: high task-completion failure, with separate toy-project quality
findings. The core happy path works, but the model's own verification suite is
internally inconsistent and the project lacks the requested documentation.

Impact: a first user following the stated evaluation command sees a failing
project even though a manual smoke path appears healthy; the agent never reached
the point where it could diagnose or repair these failures.

### O-07 — Mutation approval and durable state behaved as documented

Condition: The isolated config set `tools.auto_approve_mutating` to `true` and
the workspace was passed as `--cwd .`.

Observed: `write` and `edit` calls modified only the toy workspace, `exec` ran
the smoke commands, and `.rupi-state` recorded the session. A generated attempt
to write outside the root was refused. The case `.gitignore` kept state, Python
bytecode, and task data out of the eventual commit.

Classification: positive safety/usability observation. The explicit mutation
switch is understandable once found in the quickstart, but a new user must
manually edit JSON before any coding task can change files.

### V-08 — Workspace test command has an unrelated Windows fixture failure

Condition: The case branch contains no changes under `src/`, `crates/`, or
`tests/`; the repository verification commands were run after the case was
complete.

Expected: `cargo test --workspace --quiet` should pass on this Windows host.

Observed: The workspace suite failed in the unchanged integration test
`tests/tool_lifecycle_events.rs` test
`an_outcome_the_runtime_cannot_determine_is_not_coerced_into_success_or_failure`.
That fixture invokes `sleep 5`, but the Windows `cmd.exe` runner reports
`'sleep' is not recognized as an internal or external command`, so the test
receives `ToolFailed` immediately instead of the timeout-boundary
`ToolUnknown` outcome it is designed to exercise. Formatting, `cargo check`,
clippy, and documentation generation passed.

Exit/status: workspace test command failed; the other listed repository checks
passed.

Evidence: the failing test source is unchanged from `origin/main`, and
`git diff --name-only main...HEAD -- src crates tests` is empty. No rupi source
or test fix was made for this case.

Classification: validation limitation / pre-existing Windows test portability
issue, not a regression introduced by this case.

Impact: the repository's prescribed full test gate is not green on this host,
so the case handoff distinguishes that baseline failure from the toy-project
findings.
