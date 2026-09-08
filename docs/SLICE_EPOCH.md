# Slice: track context compaction **epochs** (not compaction itself)

ROADMAP: "Track context compaction epochs" (≈line 243). Ordinary compaction is a later, larger item
(≈line 262). This slice adds only the durable record that a compaction happened, so a future
compaction has somewhere honest to write and the trace never has to imply that a reduced model-visible
context was the canonical history.

## Scope discipline (this is the whole risk)

Add **exactly one** core event/record kind and its persistence + rendering. Do **not** implement
summarisation, thresholds, token accounting, or a `compact` command. If the scope feels too small,
that is correct.

## Contract

1. A compaction epoch record names: the session, an epoch ordinal (monotonic, starting at a defined
   value), the canonical range it **replaces in the model-visible context** (start/end record ids or
   sequence numbers — use whatever the existing records already expose, do not invent a new coordinate
   system), and what stands in for that range (an opaque reference to the summary, not the summary
   body).
2. Canonical history is **never** rewritten: the record is appended, and the replaced range stays
   readable through `pi-rs trace`. Say so in the doc comment.
3. The epoch ordinal must be derivable from the trace, not from in-memory counters, so a resume sees the
   same epoch numbering.
4. `pi-rs trace` shows the epoch in a way that makes clear the canonical trace is intact — reuse the
   existing rendering path (`trace::render_trace`), no new surface format.
5. Ordering: an epoch record must never sort before the records it summarises.

Derive the variant name, field names, and serialization from what
`crates/pi-rs-core/src/event.rs` (and the store's record encoding) already does. Follow the existing
shape; do not redesign it.

## Required tests

* unit: an epoch record round-trips through the existing event serialization;
* unit: ordinal derivation from a trace with N epoch records yields the next ordinal;
* integration: a store containing an epoch record is rendered by `pi-rs trace` without dropping any
  canonical record (assert on counts and on the replaced range still being present).

## Context discipline

At most **four** `read` calls, ≤40 lines each, with `offset`/`limit`; one `grep -n` per file with
`| head -12`; command output ≤15 lines; `cargo test --workspace` at most once, at the end;
**commit after every green step**. Never weaken an assertion to get green. Leave ROADMAP unchecked.
No merges, rebases, pushes, PRs; stay in this worktree. `git rev-parse HEAD` before each commit — if it
moved without your commit, stop and report.
