# Round 8 audit

I audited current `main` at **`859ee927602e9fe9c550ad7dae61facebe546fe7`**, including merged PR #98 and its PR-head CI run **35463598136**.

The good news is that **all four Round 7 P1 findings are materially fixed**, and the additional release-hardening work is strong. The bad news is that I found **one new P1 regression introduced while adding causal tool-event parenting**.

**Round 8 verdict: fix required — 1 P1/high-concern issue, no P0.** This is close to acceptance.

## Round 7 disposition

| Round 7 finding                                   | Round 8 status                                                                                                                                                                                           |
| ------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Provider tool-call IDs incorrectly session-global | **Closed.** New traces use causal `EventId` identity; legacy parentless traces only fall back to an unambiguous active call ID. Duplicate IDs within one provider response are rejected before emission. |
| Interactive clean exit lacks `SessionEnded`       | **Closed.** Normal interactive return now calls `session.close()`; recoverable fatal turn errors go through `close_after_failure()`.                                                                     |
| Management commands swallow sink failures         | **Closed.** `TurnError::Refused` is separated from `TurnError::Sink`; sink failures terminate the interactive handle.                                                                                    |
| Windows Ctrl-C cannot cancel an in-flight turn    | **Closed.** `SetConsoleCtrlHandler` is installed through RAII and the callback only performs the cancellation atomic store.                                                                              |

CI is green across **Ubuntu, macOS, and Windows** for fmt, clippy, tests, docs, and mdBook.

The causal tool-lifecycle change is otherwise particularly good:

```text
ModelRequestCompleted
        ↓
ToolRequested
        ↓
ToolStarted
        ↓
ToolCompleted / ToolFailed / ToolUnknown
```

That is a significantly better durable identity model than making provider-generated `call_1` globally unique.

---

# P1 — `record_unexecuted_calls()` leaves an unfinishable held `ToolFailed` transaction

This appears to be a regression from the causal-parent change.

In `crates/pi-rs-runtime/src/turn.rs`, `record_unexecuted_calls()` now does approximately:

```rust
let requested = self.emit_with_parent(
  ...,
  AgentEvent::ToolRequested(...),
  Some(assistant_event_id),
)?;

self.emit_with_parent(
  ...,
  AgentEvent::ToolFailed(...),
  Some(requested.meta.event_id),
)?;
```

The second call is the problem.

`emit_with_parent()` is an ordinary `Trace::emit()` operation.

But in `crates/pi-rs-runtime/src/store_trace.rs`, ordinary `StoreTrace::emit()` treats every `ToolFailed` as a **message-bearing transaction**:

```rust
let hold_for_message = matches!(
  &envelope.event,
  ...
  | AgentEvent::ToolFailed(_)
  ...
);
```

Consequently the store:

1. prepares a WAL transaction;
2. appends the canonical `ToolFailed`;
3. deliberately leaves the WAL intent open waiting for its corresponding semantic `Message`.

`record_unexecuted_calls()` never supplies that message.

The existing unit test actually exposes the intended contract very clearly:

```rust
trace.emit_without_message(&mut failed).unwrap();
trace.into_session().finish().unwrap();
```

`a_no_execution_tool_failure_closes_its_wal_intent()` correctly uses **`emit_without_message`**, while the real runtime path now uses ordinary `emit_with_parent`.

So on the affected runtime path:

```text
ModelRequestCompleted(failed)
↓
ToolRequested
↓
ToolFailed
↓
WAL remains pending
↓
TurnCompleted / flush
↓
"incomplete projection transaction; finish requires recovery"
```

Instead of reporting the actual model/cancellation failure, a valid live execution can degrade into a **durable sink failure** and force recovery.

### This path is practically reachable

This is not merely a theoretical misuse of an internal helper.

`Collector` can contain decoded tool calls while the provider response is ultimately classified incomplete. The runtime explicitly has:

```rust
let Collector {
  text,
  calls,
  committed,
  ...
} = collector;
```

and subsequently, on failure:

```rust
self.record_unexecuted_calls(..., &calls, ...)
```

There is already a runtime test demonstrating exactly this semantic case:

```rust
decoded_tool_call_before_context_overflow_is_not_replayed
```

using a provider that emits a complete `ToolCall` and subsequently reports a streaming failure.

The in-memory `Recorder` used by that test has no WAL semantics, so the regression remains invisible.

A real OpenAI-compatible endpoint can hit the same conceptual boundary if a complete tool-call structure is decoded but the response stream ends without a definitive completion boundary.

## Suggested fix

Add the missing parent-aware no-message primitive, e.g.:

```rust
fn emit_without_message_with_parent(
  &mut self,
  turn_id: Option<TurnId>,
  event: AgentEvent,
  parent_event_id: Option<EventId>,
) -> Result<EventEnvelope, TurnError> {
  self.emit_with_sink_parent(turn_id, event, true, parent_event_id)
}
```

Then change `record_unexecuted_calls()` to:

```rust
self.emit_without_message_with_parent(
  Some(turn_id.clone()),
  AgentEvent::ToolFailed(...),
  Some(requested.meta.event_id),
)?;
```

That preserves both required properties:

```text
causal parent:
ToolRequested → ToolFailed

semantic intent:
ToolFailed is terminal evidence only;
no ToolResult enters model-visible history
```

I would **not** switch this path to `emit_message_with_parent()` unless you explicitly want the failed/non-executed call to enter model context. The previous implementation deliberately chose `emit_without_message`, and that appears semantically correct.

### Regression test I would require

Use a **real `StoreTrace`**, not `Recorder`:

```text
provider emits a decoded ToolCall
→ provider response subsequently becomes incomplete/fails
→ tool is never executed
```

Then assert:

* `ToolFailed.parent_event_id == ToolRequested.event_id`;
* the actual failure remains the reported turn outcome rather than becoming `TurnError::Sink`;
* no pending projection WAL remains;
* `finish()` succeeds;
* `Store::restore()` succeeds immediately;
* no interrupted tool appears;
* no synthetic tool-result message unexpectedly enters model-visible history.

That would directly pin the invariant that this regression violated.

---

# P2 residuals

I found one notable hardening inconsistency.

**The provider HTTP relay now bounds blocked DNS resolution, but the MCP HTTP relay does not.**

`crates/pi-rs-provider/src/relay.rs` now has the process-wide one-worker/bounded-queue resolver introduced in PR #98.

The parallel implementation in `crates/pi-rs-mcp/src/relay.rs` still does, per lookup:

```rust
let (sender, receiver) = std::sync::mpsc::sync_channel(1);

thread::spawn(move || {
  let result = (host.as_str(), port)
    .to_socket_addrs()
    ...
});
```

Thus repeatedly cancelled MCP calls against a resolver that itself hangs can still accumulate detached resolver threads.

I consider this **P2 rather than P1** because MCP endpoints are operator-configured and the number of pathological lookups is naturally constrained, but I would eliminate the duplication. Ideally the cancellable-relay DNS/connect machinery should become one shared internal implementation used by both provider and MCP transports.

Other remaining items are similarly non-blocking: there is still no actual PTY integration test proving `/quit → SessionEnded(UserExit)` end-to-end; an interactive session quit before its first turn can produce `SessionEnded` without a preceding `SessionStarted`; and resume still rehydrates historical epoch capabilities from current provider configuration rather than persisting the original capability snapshots in `SessionEpochRecord`. Those are worthwhile follow-up hardening/design items, but I would not hold this fix on them.

## Release-hardening review

The ancillary Round 7 fixes look good:

* oversized semantic records are now rejected **before** blob/WAL/canonical mutation;
* failed message-WAL preparation conservatively cleans newly-created unreferenced recovery blobs;
* `OpenAiCompat::Debug` now redacts query/fragment URL secrets;
* provider resolver work is bounded;
* release `panic = "abort"` now has a panic hook that attempts terminal restoration;
* Cargo repository metadata now points to the correct repository.

I did not find another P1 in those areas.

# Recommendation for Round 9

This is now a very small target. I would fix the **parent-aware no-message `ToolFailed` regression**, preferably also mirror/factor the bounded resolver into MCP, then do a narrow Round 9 verification.

If the P1 is corrected and its test uses `StoreTrace` rather than an in-memory recorder, **I currently see no other highly concerning issue that should block acceptance**. Round 9 can realistically be the sign-off round.
