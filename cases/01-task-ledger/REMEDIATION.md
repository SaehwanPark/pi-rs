# Task-ledger remediation record

Date: 2026-09-20
Scope: `sol_review_01.md`, PR [#105](https://github.com/SaehwanPark/rupi/pull/105)

## Outcome

The task-ledger case now has a frozen external acceptance layer, a repaired toy
project, bounded and recoverable rupi turns, explicit Windows execution paths,
and a direct process tool for commands that should not depend on shell quoting.
The toy project passes both its generated suite and the independent subprocess
oracle.

The historical first-user prompt remains in the excluded session trace; it is
not reconstructed here. This record freezes the remediation oracle and its
results instead of claiming to reproduce that missing prompt.

## Toy-project contract repairs

- `SPEC.md` defines ASCII positive IDs, global `--state` (also accepted after a
  subcommand), row-matching list summaries, missing versus empty state-file
  behavior, and the separate-process evaluation boundary.
- The stale two-value `mark_done()` test calls and latent `.add()` tuple bug
  were corrected.
- Ledger construction enforces `next_id` above every stored ID, and an existing
  empty state file is rejected as possible truncation.
- `acceptance/test_tasklog_subprocess.py` invokes `python -m tasklog` in a new
  interpreter for every command; it is outside the model-owned test suite.

## rupi runtime repairs

- `limits.max_model_requests_per_turn` is typed, configurable, defaults to 32,
  and is bounded by a hard ceiling of 256. Progress surfaces emit sparse budget
  milestones.
- A normal turn reserves one no-tools finalization request. The turn remains
  `BudgetExhausted` and exits unsuccessfully when incomplete; `--finalize`
  provides a bounded no-tools resume assessment.
- Recoverable failures persist as `SessionEndReason::Interrupted`, with resume
  diagnostics naming the restored model/context epoch, reconciled tools, and
  fresh request budget.
- `process` runs a program with an argv list, while `exec` documents its
  platform shell (`cmd.exe /C` on Windows, `sh -c` on Unix-like systems).
- MCP, exec, and lifecycle fixtures use platform-native subprocesses. Shell
  benchmark scripts are forced to LF and select the native release artifact.

## Frozen verification commands

Run from `docs/cases/2026-09-20-task-ledger/toy-project`:

```text
python -m unittest discover -s tests -p "test_*.py" -v
```

Run from `docs/cases/2026-09-20-task-ledger`:

```text
python -m unittest discover -s acceptance -p "test_tasklog_subprocess.py" -v
```

Verified results: generated toy suite **79 passed**; independent subprocess
oracle **3 passed**.

## Independent tester loop

The required independent tester ran in a read-only workspace at checkpoint
`65f5bab`. It confirmed the 79/3 Python results and the focused request-budget,
finalization, resume, process, and lifecycle tests. Its bounded live-model run
reached the configured four-request budget honestly (`budget_exhausted` plus
`interrupted`) but was inconclusive as a project-acceptance run: the model
prefixed an already-rooted workspace path with `toy-project` and did not finish
the suite. Deterministic acceptance is therefore the authoritative evidence
for this remediation; the live result is retained as a limitation, not a
false success claim.

The same tester independently reproduced four pre-existing Windows-only
`rupi-tools` fixture failures (`touch`, `sleep`, `yes`, and `true`). Those
fixtures were replaced with equivalent platform-native commands, and the
post-fix workspace suite passes.

A follow-up parent-owned live smoke run used a disposable copy of the toy
project with a temporary 24-request budget. It completed the fresh-process
add/list/done/list-all/remove sequence, verified malformed-id and missing-
argument failures returned non-zero with stderr-only corrective messages while
the state-file SHA-256 stayed unchanged, and ran the 79-test suite through
`rupi`'s `process` tool. The run exited successfully. Two earlier six- and
12-request probes stopped at their configured boundaries before the full smoke
sequence; those are retained as budget-efficiency observations, not failures
of the project contract.

Parent invariant review also closed two final boundary gaps: direct Unix
`process` programs now run in their own process group so descendants are killed
on timeout/cancellation, and the public runtime budget builder enforces the
same 256-request ceiling as configuration validation.

## Verification evidence

On the Windows checkout after the fixes:

- `cargo fmt --all --check` — pass.
- `cargo check -p rupi-core --all-features` — pass.
- `cargo clippy --workspace --all-targets -- -D warnings` — pass.
- `cargo test --workspace` — pass, including 77 `rupi-tools` tests, 23 registry
  tests, 31 MCP unit tests, and 4 MCP integration tests.
- `cargo doc --workspace --no-deps` — pass.
- `bash bench/startup.sh --json bench/results/startup-ci.json` — pass under
  the host WSL Bash environment after the checkout's Cargo/PATH correction;
  cold 9.37 ms, warm mean 9.68 ms, warm median 9.54 ms.
