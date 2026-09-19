# Invariant Review

Verdict: pass

## Scope

Reviewed the Round 8 audit slice against the runtime, persistence, tool lifecycle,
MCP cancellation, startup, and recovery invariants. The implementation is limited to
closing the no-message `ToolFailed` transaction path and bounding MCP DNS resolver work.
It does not alter model-visible failure semantics or add orchestration policy.

## Review passes

### Pass 1 — runtime and durable-state invariants

- `TurnLoop::record_unexecuted_calls` still emits `ToolRequested` parented to the failed
  model request, then emits `ToolFailed` parented to that request event.
- `emit_without_message_with_parent` selects the existing `Trace::emit_without_message`
  boundary, so `StoreTrace` does not hold a message WAL intent for a terminal evidence-only
  event.
- The Store-backed regression drives a decoded tool call through a failed provider stream,
  preserves the original `ContextOverflow` outcome, finishes and restores the session, and
  verifies no tool-result message or interrupted call is created.

### Pass 2 — error, side-effect, and transport review

- The fix does not execute or replay the decoded call; its terminal state remains failed
  evidence without model-visible tool-result content.
- A failure while writing the requested or terminal event still propagates as a sink error,
  preserving the existing recovery barrier rather than being treated as provider success.
- MCP resolution now has one process-wide worker and one queued job. Cancellation and
  deadlines continue polling the per-call result channel; a full or disconnected queue fails
  closed instead of creating another detached resolver thread.
- IP literals retain the existing direct path, and the platform resolver remains responsible
  for OS DNS policy.

### Pass 3 — scope, compatibility, and verification review

- The diff introduces no schema migration, provider-wire change, startup connection, or
  model-epoch behavior change.
- The MCP change mirrors the already-verified provider relay policy without widening the
  relay abstraction or changing HTTP request semantics.
- No actionable blocking finding remains.

## Findings

No blocking findings.

## Checks confirmed

- Causal parent identity is asserted from the durable trace journal.
- `Store::finish` and immediate `Store::restore` are exercised on the affected path.
- Restored model-visible messages contain no synthetic tool result.
- MCP local-name resolution remains covered by its resolver test.
- Unknown/uncertain mutating-tool semantics and sink-failure recovery barriers are unchanged.

## Verification evidence

- `cargo fmt --all --check`
- `cargo check -p pi-rs-core --all-features`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test -p pi-rs-runtime decoded_tool_call_before_stream_failure_closes_store_wal -- --nocapture`
- `cargo test -p pi-rs-mcp relay::tests -- --nocapture`
- `cargo test --workspace`
- `cargo doc --workspace --no-deps`
- `bash bench/startup.sh --json bench/results/startup-ci.json` (run with a local
  `python3` shim because this Windows environment's `python3` command is a Microsoft Store
  stub; cold 135.37 ms, warm median 6.02 ms)
- `git diff --check main...HEAD`

## Residual risk

The provider and MCP relays still contain intentionally mirrored resolver implementations;
future policy changes must update both. Shared factoring is a follow-up, not a blocker for
this bounded audit fix.
