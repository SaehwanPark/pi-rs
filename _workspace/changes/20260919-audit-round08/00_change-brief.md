# Round 8 audit fixes

## Request

Address `audits/20260919/round08.md` against `main` (`859ee927`). Open a draft PR early,
keep progress visible through incremental commits, and merge the verified change into
`main`.

## Vertical slice

1. Preserve causal `ToolRequested -> ToolFailed` parenting when a decoded provider tool
   call is recorded as unexecuted, while closing the durable no-message projection
   transaction.
2. Add a regression test using a real `StoreTrace` that exercises an incomplete provider
   response, verifies the actual provider failure remains the turn outcome, and confirms
   immediate finish/restore with no synthetic tool-result message.
3. Bound MCP relay DNS resolver work with the same one-worker/one-queued-job policy as the
   provider relay, avoiding a detached resolver thread per cancelled lookup.

## Owned paths

- `_workspace/changes/20260919-audit-round08/`
- `crates/rupi-runtime/src/turn.rs`
- `crates/rupi-runtime/src/turn.rs` (tests)
- `crates/rupi-mcp/src/relay.rs`
- relevant documentation/tests only when required by the contract

## Non-goals

- no change to model-visible semantics for unexecuted tool calls;
- no replay of uncertain or mutating operations;
- no broad relay abstraction or transport redesign;
- no unrelated audit cleanup.

## Acceptance evidence

- parent-aware no-message emission is used for unexecuted `ToolFailed` events;
- a real store-backed regression test proves the WAL intent closes and `Store::restore`
  succeeds without adding a tool-result message;
- MCP relay resolution is bounded to one worker plus one queued request and remains
  cancellation/deadline aware;
- formatting, focused tests, workspace clippy/tests/docs, and the relevant benchmark or
  CI evidence pass;
- invariant review records no blocking findings before merge.
