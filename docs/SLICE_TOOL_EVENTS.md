# Slice: prove that every tool action leaves a durable lifecycle event

Phase 4's gate includes "All tool actions produce durable lifecycle events" (ROADMAP). Nothing in
the repo demonstrates it end-to-end. This slice makes the claim checkable.

## Orient with grep, do not read whole files
- `grep -n "Tool\\|Unknown\\|pub enum" crates/pi-rs-core/src/event.rs | head -40` — the lifecycle
  variants and their payloads.
- `grep -rn "CARGO_BIN_EXE\\|Command::new" tests/run_cli.rs | head -10` — how a CLI-level test
  drives the real binary, and how existing tests point `--store` at a temp dir.
- `grep -rn "ToolCompleted\\|ToolStarted\\|fn drive\\|fake" src/run/tests.rs | head -20` — how a
  turn with tools is driven deterministically (fake provider).

## Deliverable: `tests/tool_lifecycle_events.rs`
Drive the real binary with the deterministic fake provider through a turn that performs **more than
one** tool action, then read the durable trace from the store and assert:
1. every tool action carries a **durable ID**, and the same ID ties the action's records together;
2. lifecycle records are **deterministically ordered**: a start precedes its terminal record, and
   there is exactly one terminal record per ID;
3. a **failing** tool records a failure — it never appears as a bare success;
4. an uncertain mutating outcome is recorded as the runtime's **unknown/uncertain** state and is not
   coerced into success or failure. If the runtime cannot reach that state on this path, prove what
   it records instead and say so in the test name;
5. the events survive a **fresh read of the trace file** (durability, not in-memory bookkeeping).

## If you find a gap
Report it exactly: `file:line`, which invariant is violated, and what is missing. Do **not** weaken
an assertion to make it pass, and do not mark a tool action compliant because a test happens to be
green. A small fix that is only "emit the event that should already have been emitted" is in scope;
anything needing design (new lifecycle state, new storage shape) is a finding to report, not to
implement. An `#[ignore]`d test is acceptable only when the gap is real and you name it in the
ignore reason.

## Constraints
- No new dev-dependencies (the repo has none for integration tests; use `std::process::Command`,
  `env!("CARGO_BIN_EXE_...")`, `tempfile` only if already a dev-dep — check `Cargo.toml`).
- Respect the redaction path; do not add raw provider payload capture.
- 2-space indent, 100 columns, edition 2024. Do not touch `crates/pi-rs-tui` or `src/interactive.rs`
  (another slice is live there).
- ROADMAP: leave the gate box **unchecked**; annotate it with what this test now proves and what it
  does not.

## Verify (paste real numbers)
`cargo fmt --all --check` · `cargo clippy --workspace --all-targets -- -D warnings` ·
`cargo test --workspace` (baseline on this branch: **492 passed**) ·
`cargo test --test tool_lifecycle_events`

Commit `--allow-empty -m "chore: start"` first, then commit after every step, always compiling. Keep
every command output under 20 lines. Do not merge, rebase, push, open PRs, or touch other branches.
