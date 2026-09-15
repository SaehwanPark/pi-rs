# Slice: `pi-rs interactive`

A session that holds many turns, driven from a terminal. Closes Phase 3's gate:
*"a user can hold a session across turns in one process."*

## Existing pieces (do not rewrite)
- `pi_rs_tui::editor::{Editor, Intent, Outcome}` — text, cursor, history, `apply(Intent) -> Outcome`, `display(prefix) -> Layout`.
- `pi_rs_tui::keys::intent(&Event) -> Intent` — keymap. Maps Ctrl-C to `Intent::Noop` **on purpose**.
- `run::open_session(config, cwd, surface, turns)` — opens a durable session and hands a
  `SessionHandle` to a closure that may call `handle.turn(prompt)` many times, then `close()`.

## Required behaviour
1. `pi-rs interactive [--config P] [--cwd D]` (see `src/cli.rs` `Command`, `parse`; keep that diff tiny —
   `feat/pi-import` collides with it).
2. Enter raw mode; read crossterm events; draw `Editor::display(prefix)` plus ONE status line
   (model id, turn state); redraw only when something changed.
3. Map events through `keys::intent`. Intercept **Ctrl-C before** calling `intent`: empty buffer
   quits; non-empty buffer must not discard text and must not quit.
4. `Intent::Submit` runs exactly one turn through the open `SessionHandle`, so context carries
   across turns. A turn failure ends the program with a non-zero exit after restoring the terminal.
5. Restore the terminal on **every** path, including errors and panic-unwinding callers: scope the
   raw-mode guard so it cannot be skipped.
6. Turn interruption/cancel is OUT of scope (separate runtime slice). Quit is the only exit.

## Constraints
- `pi-rs-tui` must not import runtime/store; the loop lives in the composition root (root crate).
- No new crate. crossterm is already a root dependency (workspace 0.28).
- Nothing heavy before the first frame: `bash bench/startup.sh` must be unchanged.
- Put decisions in PURE tty-free functions — `event -> LoopAction`, and status-line content
  selection — and unit-test those; no test may require a tty.
- 2-space indent, 100 columns, edition 2024.

## Verify (paste real numbers)
`cargo fmt --all --check` · `cargo clippy --workspace --all-targets -- -D warnings` ·
`cargo test --workspace` · `bash bench/startup.sh`

Commit after every step, always compiling. If cut off, commit the compiling subset and say what
is missing.
