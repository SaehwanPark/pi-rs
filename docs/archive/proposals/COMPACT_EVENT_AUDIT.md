# Compaction event audit (2026-09-07; historical snapshot)

Scope: which context-compaction variants in `crates/rupi-core/src/event.rs` anything actually
constructed at the time. The findings and line references below record the pre-producer audit;
its resolution is documented at the end of this file. Runtime producers were added later, and no
variant was deleted.

## Variants

`AgentEvent` carries exactly two context-compaction variants
(`grep -n -i compact crates/rupi-core/src/event.rs`):

- `ContextCompactionStarted` — `crates/rupi-core/src/event.rs:255`, payload struct at `:462`
- `ContextCompactionCompleted` — `crates/rupi-core/src/event.rs:262`, payload struct at `:468`

`ContextReduced` (`crates/rupi-core/src/event.rs:248`, struct at `:449`) is **not** part of this
pair: it is payload-level collapsing of one item, it carries no `ContextLevel`, and unlike the
compaction pair it does have production producers —
`crates/rupi-runtime/src/turn.rs:921` and `crates/rupi-runtime/src/turn.rs:1154` (both outside the
`#[cfg(test)]` module that starts at `crates/rupi-runtime/src/turn.rs:1441`).

## Producer findings

Searched with `grep -rn "<Variant>" --include=*.rs src crates tests` over the whole workspace.

| Variant | Production producer | Test-only construction | Consumers |
| --- | --- | --- | --- |
| `ContextCompactionStarted` | **no producer found** | `crates/rupi-core/src/event.rs:615`; `crates/rupi-tui/src/transcript.rs:1240` in `all_events()` at `:1130` (test module at `:712`) | `crates/rupi-tui/src/transcript.rs:419` |
| `ContextCompactionCompleted` | **no producer found** | `crates/rupi-tui/src/transcript.rs:1244` in `all_events()` | `crates/rupi-tui/src/transcript.rs:162`, `crates/rupi-tui/src/transcript.rs:426` |

The only remaining hits are the variant definitions themselves and the re-export list in
`crates/rupi-core/src/lib.rs:54-55`. `crates/rupi-tui/src/transcript.rs:419` and `:426` are render
arms (they read the variant, they do not build it).

## Why nothing emits them

The runtime reaches the point where compaction would happen and deliberately stops. At the safe
boundary — `crates/rupi-runtime/src/turn.rs:951` evaluates the context policy; the policy can
recommend `ContextAction::Compact` (`crates/rupi-core/src/context.rs:132`, produced at
`crates/rupi-core/src/context.rs:321` and `:373`). The runtime matches that action at
`crates/rupi-runtime/src/turn.rs:968` and emits a `Diagnostic` warn instead of a compaction event;
the comment at `crates/rupi-runtime/src/turn.rs:965-967` states the reason: compaction needs a
model and a user decision, and the surface owns that.

## Shape coverage

No file in the repo contained the literal strings `context_compaction_started` or
`context_compaction_completed` before this audit (`grep -rn "context_compaction" --exclude-dir=target
--exclude-dir=.git .` returned nothing), and the only existing test
(`compaction_events_carry_level`, `crates/rupi-core/src/event.rs:614`) asserted on the level string
alone — not the `type` tag and not the field names.

Pinned by two new unit tests in `crates/rupi-core/src/event.rs`:

- `compaction_started_wire_shape_is_pinned` (`crates/rupi-core/src/event.rs:630`) — exact JSON
  `{"type":"context_compaction_started","level":"l1_ordinary","reason":"…"}`
- `compaction_completed_wire_shape_is_pinned` (`crates/rupi-core/src/event.rs:647`) — exact JSON
  `{"type":"context_compaction_completed","level":"l2_phase","removed_messages":12,"retained_messages":30,"context_epoch":2}`

Both round-trip through `serde_json::from_str` and compare equal to the original event.

## Resolution and Implementation

Issue #38 evaluated whether to emit the compaction event family or remove the variants from the schema.
Resolution 1 was adopted: compaction producers were implemented in `crates/rupi-runtime/src/turn.rs`
across three compaction mechanisms:

1. **L1 Summarizing Compaction** (PR #56): emits `ContextCompactionStarted(Level::L1Ordinary)` →
   `ContextSummary` → `ContextCompactionEpoch` → `ContextCompactionCompleted(Level::L1Ordinary)`.
2. **L2 Semantic Phase Compaction** (PR #61): emits `ContextCompactionStarted(Level::L2Phase)` →
   `ContextCompactionCompleted(Level::L2Phase)`.
3. **L3 Checkpoint Compaction** (PR #59): creates impermeable barriers and advances context epoch via
   `ContextCompactionCompleted(Level::L3Checkpoint)`.

All variants are actively constructed in production, tested with round-trip serialization tests, and
rendered in the transcript and interactive TUI.
