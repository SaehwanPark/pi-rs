# Slice: the two lifecycle cases #29 left out

`tests/tool_lifecycle_events.rs` already proves a durable ID ties a tool action's records together and
that each ID has exactly one terminal record, ordered after its start. Add **two** cases, same file,
same driving style — copy what the existing tests do; do not redesign them:
1. a **failing** tool records a failure: its terminal record is not a bare success;
2. an outcome the runtime cannot determine is **not coerced** into success or failure. If this path is
   unreachable from the CLI with the deterministic provider, prove what *is* recorded and name that in
   the test instead of asserting a state the runtime never reaches.

Grep the existing file for how it builds the store, invokes the binary and reads the trace:
`grep -n "fn \|CARGO_BIN_EXE\|Command::new\|read_dir\|serde_json" tests/tool_lifecycle_events.rs | head -20`.
Then `grep -n "Unknown\|Failed\|pub enum" crates/pi-rs-core/src/event.rs | head -20`. Two reads, then write.

Discipline: reads ≤40 lines, command outputs ≤15 lines, `cargo test --test tool_lifecycle_events`
after each edit, commit per case. Never weaken an assertion to get green — a reported gap with
`file:line` is a pass. Baseline on this branch: **494 passed**. Leave the ROADMAP gate unchecked.
No merges, rebases, pushes, PRs; stay in this worktree. If HEAD moves without your commit, stop.
