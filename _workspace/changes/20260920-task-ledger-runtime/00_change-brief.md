# Task-ledger runtime recovery slice

Status: active
Owner: parent agent
Base: `main`
Branch: `fix/task-ledger-runtime`

## Request

Address the evidence and recommendations in
`docs/cases/2026-09-20-task-ledger/sol_review_01.md` so a first-time local
`rupi` user can complete and verify the task-ledger toy project without losing
the distinction between partial work and success.

## Vertical slice

1. Make the per-turn request budget a typed, validated runtime configuration
   with the existing safe default retained.
2. Surface sparse request-budget progress in the normal headless transcript.
3. Record recoverable turn termination as resumable/incomplete rather than
   `Fatal`, and print a compact recovery summary before a resumed request.
4. Add a bounded `--finalize` resume mode that permits a no-tool completion
   assessment while preserving a non-success status when the prior turn was
   incomplete.
5. Make the built-in shell contract explicit, especially on Windows, and add
   focused lifecycle/configuration coverage.
6. Repair the task-ledger acceptance evidence: freeze a subprocess smoke
   oracle, clarify the toy specification, and keep generated-suite failures
   separate from acceptance failures.

## Acceptance criteria

- Existing configurations without `limits` retain a 32-request default.
- Invalid or excessive request limits fail before a provider request.
- Request milestones are visible without enabling verbose routine output and
  remain absent for ordinary early requests.
- A budget-exhausted one-shot run exits non-zero, records an explicit
  recoverable session end, and can be resumed; `--finalize` makes no tool calls
  and remains non-successful when no final answer was produced.
- A resumed run reports the restored state and fresh request budget before
  contacting the provider.
- The model-facing `exec` schema identifies the active platform shell.
- The task-ledger subprocess acceptance path passes on a clean temporary state
  directory and invalid operations preserve state bytes.

## Owned paths

- `crates/rupi-core/`, `crates/rupi-runtime/`, `crates/rupi-tui/`
- `src/`, `tests/`, `book/`
- `docs/cases/2026-09-20-task-ledger/` acceptance evidence and toy contract
- this change directory and the relevant `ROADMAP.md` entry

## Explicit non-goals

- Removing the runaway-loop safety cap.
- Changing the model/provider or hidden-reasoning semantics.
- Adding a multi-agent or workflow orchestration layer.
- Claiming a beginner reasoning-level change without a controlled benchmark.
- Re-running arbitrary mutating tools during finalization or recovery.

## Verification plan

Focused Rust unit/integration tests first; then the repository checks from
`AGENTS.md`, the frozen task-ledger subprocess acceptance suite, and an
independent tester pass against the toy project. Any tester finding that
blocks completion will receive one focused root-cause revision pass.
