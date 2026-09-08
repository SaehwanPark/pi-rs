# Contributing to pi-rs

pi-rs is a coding-agent runtime in Rust. The rules that actually decide whether a
change is accepted live in [`AGENTS.md`](AGENTS.md), and the plan lives in
[`ROADMAP.md`](ROADMAP.md). This file is the short operational version: what to run,
what shape a change should take, and what reviewers look at first.

## Start here

Read, in order: [`AGENTS.md`](AGENTS.md),
[`docs/PROJECT_DESIGN_CANONICAL.md`](docs/PROJECT_DESIGN_CANONICAL.md),
[`ARCHITECTURE.md`](ARCHITECTURE.md), [`COMPATIBILITY.md`](COMPATIBILITY.md), then
[`ROADMAP.md`](ROADMAP.md). Most disputes about scope are settled by the first two; most
disputes about shape by the next two.

## One slice, one pull request

A pull request is a **bounded, decision-complete slice**: one behaviour, one fix, one
benchmark, one document that changes what a reader would do differently.

- Prefer several small PRs to one that mixes a fix, a refactor, and a feature. A reviewer
  can hold a slice in their head; they cannot hold a subsystem.
- State in the description what did **not** change. "Provider, store, and TUI untouched"
  is worth a paragraph of prose about what did.
- If a change depends on an unmerged PR, open it against that branch and say so: name the
  base branch and the merge order. A stacked PR whose base is a mystery is a stacked PR
  that cannot be merged safely.
- Tick the roadmap line the slice satisfies. Do not tick a stage gate until the gate's own
  evidence exists.

## What reviewers check first

These are the invariants with the highest cost of being wrong, and they are the ones most
often silently broken by a change that compiles.

1. **Provenance.** Native reasoning, provider summaries, declared rationale, and
   reconstructed rationale never collapse into each other, in memory, on the wire, or in
   the session file. Nothing claims hidden chain-of-thought was recovered.
2. **Canonical history.** Context reduction may shrink what a model sees; it may not lose
   what happened. A reduced trace still replays to the same session.
3. **Unknown is not failure.** A mutating tool whose completion is uncertain stays
   `Unknown`. Uncertain operations are never replayed blindly.
4. **Startup latency.** No network call, no directory scan, no Node, no eager MCP
   connection, no index open before the user could have typed. The question to ask before
   adding anything to the startup path: *must this happen before the first prompt?*
5. **Redaction.** Durable trace output passes through the redaction policy. Raw provider
   payload capture stays opt-in.
6. **Compatibility is behavioural.** Pi parity is proven by a fixture, not by an
   assertion that the shapes look similar. Silence in a compatibility report is only for
   what Pi deliberately ignores; every other decision names a path and a reason.
7. **Layer boundaries.** `pi-rs-tui` does not import the runtime or the store.
   `pi-rs-compat` imports nothing from this workspace. MCP stays behind its adapter.

## Running the checks

CI runs on `ubuntu-latest` and `macos-latest`:

```sh
cargo fmt --all --check
cargo check -p pi-rs-core --all-features
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo doc --workspace --no-deps
bench/startup.sh --json bench/results/startup-ci.json
```

Run them the way CI does, before asking for review. Two flags matter and are easy to lose
locally: `--all-targets` (tests and benches are linted too) and `-D warnings` (a warning is
a failure). Rustdoc warnings fail review as well, so the `cargo doc` pass is not optional.

Changes on the startup path, or to rendering, session resume, context reconstruction,
package discovery, MCP first use, or extension-host startup, need a benchmark. Latency
budgets live in the bench source that enforces them, and they are a pre-merge gate, not a
CI gate: a five-times-some-machine baseline is generous locally and meaningless on a shared
runner.

## Style

Edition 2024, `rust-version = 1.85`, two-space indent, 100-column lines, idiomatic stable
Rust. Small focused modules, typed state instead of boolean conventions, typed errors, I/O
at the boundary with transformation logic that is pure and testable. `unsafe` needs an
architecture-level justification, not a performance hunch.

Every new dependency needs a concrete need, an ecosystem-maintained crate, an acceptable
startup and binary cost, and no standard-library or local alternative. Dependencies on the
startup path get extra scrutiny. A reason is expected when it is not obvious.

## Tests

For any non-trivial change: unit tests for the pure logic, integration tests at the
boundary where behaviour is decided, failure-path tests, and a compatibility fixture when
Pi behaviour is involved. Failover changes include a mid-turn failure case. Context changes
prove canonical trace preservation. Event changes prove deterministic ordering.

A pull request is not done because it compiles. It is done when tests, compatibility,
trace and provenance correctness, performance expectations, failure behaviour, and
documentation all say so, and `ROADMAP.md` reflects reality.
