# Slice: interrupt a running turn

`pi-rs interactive` can only quit. Ctrl-C while a turn is running is a plain SIGINT: the process
dies, the closing flush is lost, and the session cannot be used again. The runtime already polls
a `CancelToken` cooperatively (see the poll sites in `crates/pi-rs-runtime/src/turn.rs` and
`crates/pi-rs-provider/src/openai.rs`); the token is simply not reachable from a caller — the
composition root constructs one internally. Make it reachable, then wire the loop.

## Required
1. A caller can supply the `CancelToken` for a turn on `run::SessionHandle` (new method or a
   token on the handle — your call, keep `run::execute` behaviour byte-for-byte unchanged).
2. The interactive loop owns one token, sets it on Ctrl-C **while a turn is in flight**, and
   leaves Ctrl-C-on-idle-prompt to quit (that rule already exists; do not move it).
3. A canceled turn ends in a defined state, the session stays usable, and the next turn still
   works. Cancellation is not silently rewritten as success or as provider failure.
4. A mutating tool call whose completion is uncertain stays `Unknown`. Never replay it.
5. The canonical trace records the cancellation as a semantic event, not a log line.

## Constraints
- No new crate. `pi-rs-tui` imports neither runtime nor store. Keep `src/cli.rs` diffs small.
- Startup path unchanged: `bash bench/startup.sh` must not move.
- The interrupt decision (idle → quit, running → cancel) must stay a pure tty-free function in
  `src/interactive.rs` and be unit-tested. No test may need a tty.
- 2-space indent, 100 columns, edition 2024.

## Tests expected
- Canceled turn terminates promptly and the SAME handle runs a following turn successfully.
- Cancellation mid-tool-call does not fabricate a result; state is `Unknown`.
- Existing one-shot `run` tests keep passing unchanged.

## Verify (paste real numbers)
`cargo fmt --all --check` · `cargo clippy --workspace --all-targets -- -D warnings` ·
`cargo test --workspace` · `bash bench/startup.sh`

Commit `--allow-empty -m "chore: start"` first, then commit after every step, always compiling.
If cut off, commit the compiling subset and say what is missing. Do not merge, rebase, push,
or open PRs.
