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
