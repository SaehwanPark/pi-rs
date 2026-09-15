# Phase 10 — Replay and research tooling

## Scope

Add a pure `pi-rs-replay` projection boundary and the read-only `pi-rs replay` CLI. The
implementation consumes redacted canonical trace entries and optional semantic session records;
it never starts a provider, executes tools, or treats historical events as a new generation.

## Contracts

- Sequence numbers, not timestamps, define event order and inclusive replay-until selection.
- Tool, reasoning, and timing filters are deterministic projections over the same history.
- Canonical session messages remain separate from the model-visible working context; checkpoint
  capsules and compaction epochs are explicit working-set transformations.
- Historical branch plans are dry and visibly separate from future generation. Mutating or
  uncertain tool calls remain reconciliation barriers; no side effect is blindly replayed.
- Continuation comparison is structural only and does not judge output quality.
- Epoch, compaction, failover, and provenance views preserve model attribution and explicit
  provenance. Reconstructed rationale is never presented as recovered hidden reasoning.
- Trace export keeps normalized/redacted envelope facts and omits raw payload pointers/content.

## Verification

- `cargo test -p pi-rs-replay --all-features`
- `cargo test -p pi-rs --test replay_cli --all-features`
- `cargo test --workspace --all-features -- --test-threads=1`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo doc --workspace --no-deps`
- invariant review and required CI on PR #82
