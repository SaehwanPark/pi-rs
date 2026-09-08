# Slice: pin the compaction event shapes, and say which have producers

`crates/pi-rs-core/src/event.rs` carries more than one context-compaction variant. One of them was
added recently, one was already there, and nobody has established which of them anything actually
**constructs**. That ambiguity is worse than a missing feature: a schema variant with no producer reads
like a guarantee.

## Do this

1. Enumerate every context-compaction variant in `crates/pi-rs-core/src/event.rs` (one grep with
   `head -12`).
2. For each, determine whether anything in `src/`, `crates/` (excluding the variant's own definition),
   or `tests/` **constructs** it. Record `file:line` for producers, or "no producer".
3. Add unit tests pinning the serialized shape (`type` tag string, field names) of each variant, so the
   wire format cannot drift silently. Match the style of the existing serialization tests in that file —
   read one of them, ≤40 lines, and copy it.
4. Write the producer findings into a short doc comment on the variant group, or into
   `docs/COMPACT_EVENT_AUDIT.md` if the comment would be too long. State facts you checked, with
   `file:line`. **Do not** implement a producer, and **do not** delete a variant.

## Explicitly out of scope

No emission wiring, no compaction algorithm, no thresholds, no ROADMAP gate changes. If you find that a
variant is dead, the correct output is the sentence "no producer found at <date>" plus the tests that
pin its shape — a follow-up decides what to do about it.

## Context discipline

At most **four** `read` calls, ≤40 lines each; one `grep -n` per file with `| head -12`; command output
≤15 lines; `cargo test --workspace` at most **once**, at the end; commit after every green step. Never
weaken an assertion. No merges, rebases, pushes, PRs; stay in this worktree. `git rev-parse HEAD` before
each commit — if it moved without your commit, stop and report.
