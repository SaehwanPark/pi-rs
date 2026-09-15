# Slice: Ctrl-C interrupts the turn, not the process

Today the loop quits on Ctrl-C with an empty buffer, and a Ctrl-C **while a turn is running** is a
plain SIGINT: the process dies and the closing flush is lost. PR #24 added
`SessionHandle::turn_with(prompt, &CancelToken)`. This slice connects them. Nothing else.

## Required
1. The loop owns ONE `CancelToken` for the session's lifetime. Start a turn with `turn_with` and
   that token; before the next turn, hand out a fresh token (the old one stays set).
2. A second thread is NOT required if the loop can set the token on the same thread while it polls
   events — but a turn blocks the loop, so: the key event that means "interrupt" must be observable
   while the turn runs. Choose the smallest honest mechanism (e.g. read events during the turn via a
   poll loop, or a reader thread that only sets the token). Say which you chose and why, in code.
3. Ctrl-C semantics, final: **turn running → cancel the turn, keep the session, keep the buffer text**;
   **idle, empty buffer → quit**; **idle, non-empty buffer → nothing destructive**. Do not change the
   idle rules that already exist and are tested.
4. After a cancel, the session must still answer the next turn (PR #24 proves this at the handle
   level — the loop test proves the loop keeps going).
5. Terminal is restored on every path, cancel included.

## Hard constraints
- Keep the interrupt decision a PURE tty-free function in `src/interactive.rs`, unit-tested with no
  tty: `(turn_in_flight, buffer_empty) -> InterruptAction::{Cancel,Quit,KeepText}` or equivalent.
- Do not modify `src/run.rs`, `crates/pi-rs-runtime`, `crates/pi-rs-provider`, or `src/cli.rs`.
- No new crate. `pi-rs-tui` imports neither runtime nor store. Startup path untouched:
  `bash bench/startup.sh` must not move.
- Keep the diff small. This is a wiring slice, not a rewrite: if you feel the urge to restructure
  `interactive.rs`, stop and report instead.

## Verify (paste real numbers)
`cargo fmt --all --check` · `cargo clippy --workspace --all-targets -- -D warnings` ·
`cargo test --workspace` · `bash bench/startup.sh`

Commit `--allow-empty -m "chore: start"` first, then commit after every step, always compiling.
If cut off, the last compiling commit is the deliverable. Do not merge, rebase, push, or open PRs.
