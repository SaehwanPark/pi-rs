# Round 6 audit

I audited current `main` at **`11a48fe6543cf08bba4dc1ed585848502cc85db9`**, including merged PR #96. Its CI is fully green on Ubuntu, macOS, and Windows.

This is the strongest state of the project so far. The five Round 5 P1 findings are materially fixed, and the durability model is now much more coherent. The narrower failpoint pass finds **two remaining P1/high-concern crash windows**, both around the boundary between model-visible messages and tool lifecycle events.

**No P0/critical issue found. Two P1s remain.**

## Round 5 disposition

| Round 5 finding                                 | Round 6 disposition                                                                                                                          |
| ----------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- |
| Torn JSONL tails defeat recovery                | **Closed.** Valid unterminated tails are newline-normalized; invalid final fragments are truncated; interior corruption remains fail-closed. |
| Blob reference can outlive blob durability      | **Closed.** Temp bytes are synced, publication is verified, directory sync is performed where supported, and temp names are write-scoped.    |
| Incomplete compaction/checkpoint bricks session | **Closed.** L1/L2 attempts can be durably aborted while preserving old context; incomplete L3 boundaries are deterministically completed.    |
| Lease marker crash-poison                       | **Closed.** Lock-file existence now establishes the format; acquisition no longer destructively rewrites it.                                 |
| Credentials serializable/debuggable             | **Substantially closed.** Endpoint/provider/MCP config types now structurally redact secrets and reject URL userinfo.                        |

The new tests around interrupted compaction, prefix checkpoints, prepare-only checkpoint WAL recovery, torn tails, and lease marker recovery are meaningful rather than superficial.

---

# P1-1 — Message-bearing events are still not crash-atomic, and several cannot be reconstructed

This is now the principal durability issue.

The runtime still models a durable model-visible message as **two calls**:

```rust
let envelope = self.emit(turn_id, event)?;
self.trace.record_message(&AttributedMessage {
  envelope: envelope.clone(),
  message: message.clone(),
})?;
```

For `StoreTrace`, the first call prepares the WAL and durably writes the canonical event. The second appends the exact `SessionMessage` and commits the transaction.

So this window still exists:

```text
WAL prepare
canonical event durable
        ↓ process dies
semantic/model-visible message not written
```

The WAL detects that situation, but `recover_projection_record()` explicitly cannot reconstruct several common message types.

### Case A: successful tool result

A normal successful tool currently produces a canonical `ToolCompleted` carrying roughly:

```text
call id
tool name
status
duration
reduced flag
optional blob
visible byte count
```

It does **not** carry the ordinary model-visible result text.

The optional blob is only populated for oversized/reduced output. Therefore:

```text
tool actually succeeds
→ ToolCompleted durable
→ crash
→ ToolResult SessionMessage never written
→ resume: "manual recovery required"
```

This can occur after a mutating tool has conclusively succeeded, even though the world and canonical lifecycle are no longer ambiguous.

### Case B: external context

`ExternalContextRetrieved` records source/provenance/citation/size/metadata, but the model-visible formatted evidence itself lives in the separate message projection.

A crash between those writes loses information required to recreate the exact request context. Recovery again refuses.

### Case C: assistant response containing tool calls

This one is particularly relevant to an agent harness.

After the provider completes a tool-call response, the runtime durably emits:

```text
ModelRequestCompleted { tool_calls: N, ... }
```

The exact calls—IDs, names, and arguments—are then placed in the separately persisted assistant `SessionMessage`.

If the process dies between those writes, the canonical completion only says that tool calls existed. `recover_projection_record()` correctly refuses to invent their arguments.

So ordinary tool-using model output can still leave a permanently non-resumable session at a filesystem-write boundary.

### Recommended fix

I would now change the abstraction rather than add more event-specific reconstruction.

The durable API should make a model-visible event/message **one logical transaction**:

```rust
trace.emit_message(&mut envelope, &message)?;
```

For the store implementation:

```text
1. serialize + redact exact model-visible message
2. durably persist recovery payload
   - small: bounded inline WAL payload, or
   - large: the now-durable BlobStore
3. WAL prepare containing event identity + message recovery ref
4. append canonical event
5. append SessionMessage
6. WAL commit
```

The WAL does not need to duplicate arbitrary messages. A bounded content-addressed recovery reference is enough.

Then recovery after step 4 simply recreates the exact `SessionMessage` from its recovery payload.

This should cover all message-bearing events uniformly instead of maintaining an increasingly fragile switch such as:

```text
UserMessage → reconstruct
ToolFailed → reconstruct
ToolUnknown → reconstruct
ToolCompleted → can't
ExternalContext → can't
assistant text → derive from deltas
assistant tool calls → can't
...
```

### Required failpoint tests

This is where I would finally introduce an actual transaction failpoint matrix. Kill after prepare, canonical append, and semantic append for at least:

* user message;
* external-context message;
* assistant text;
* assistant message containing one/multiple tool calls;
* normal `ToolCompleted`;
* reduced `ToolCompleted`;
* `ToolFailed`;
* `ToolUnknown`.

For every committed canonical event, reopening should either produce the **exact same model-visible message** or a deliberate safe rollback—not manual repair for an ordinary runtime-generated state.

---

# P1-2 — A durable assistant tool call can exist without `ToolRequested`, even though non-execution is provable

There is a second, independent gap immediately after the first.

Current successful tool-call flow is effectively:

```text
ModelRequestCompleted
assistant SessionMessage containing ToolCall blocks
    ↓ crash here
execute_calls()
  ToolRequested
  ToolStarted
  actual tool code
```

After the assistant message is durably projected, `execute_calls()` begins emitting each `ToolRequested`.

If the process dies before that happens, resume sees:

```text
assistant: call tool X(args...)
```

but no canonical:

```text
ToolRequested(X)
```

`validate_projection_alignment()` treats that as corruption and rejects the session:

> assistant tool call … has no canonical ToolRequested event; resume requires recovery

Yet this state is actually safer than the `ToolRequested → ToolStarted` cases already recovered today.

The runtime's ordering contract establishes:

```text
ToolRequested must precede ToolStarted
ToolStarted must precede tool code
```

Therefore:

> **assistant ToolCall exists + no ToolRequested exists = the tool definitely did not execute through the normal runtime.**

No side-effect reconciliation is required.

This also affects multi-tool responses:

```text
assistant requests A, B, C
A executes successfully
        ↓ crash before B's ToolRequested
```

A should remain committed while B and C are safely closed as never executed.

### Recommended fix

During resume, before final projection validation, reconcile assistant calls against canonical tool lifecycles.

For each assistant `ToolCallBlock` with no matching `ToolRequested`:

```text
synthesize canonical ToolRequested
→ mark it explicitly recovery-generated / parented to assistant event
→ append ToolFailed or interrupted-before-execution terminal event
→ append corresponding ToolResult SessionMessage:
   "not executed: process stopped before execution boundary"
```

No tool code should run, and there should be **no automatic retry**.

If risk classification cannot be reconstructed from durable history, use the currently registered tool's metadata only for reporting, not for deciding whether execution happened. The absence of `ToolRequested`/`ToolStarted` is the decisive fact.

Tests should include:

```text
1 call → crash before ToolRequested
3 calls → A completed, crash before B
crash after ToolRequested but before ToolStarted
crash after ToolStarted
```

Those four states should respectively resolve to:

```text
never executed
A retained + B/C never executed
existing unstarted-request recovery
existing reconciliation/manual-inspection rules
```

That would make the tool lifecycle genuinely continuous across every boundary.

---

# Lower-priority findings

These do not currently keep me from accepting the architecture once the two P1s above are addressed.

**P2 — There still isn't a general deterministic filesystem failpoint harness.** PR #96 adds good targeted synthetic tests, but I did not find a reusable `kill after durable operation N` mechanism. At this point, adding one would probably pay for itself: wrap durable operations in a test-only failpoint seam and run representative turns through every interruption index.

**P2 — `OpenAiCompat::Debug` still exposes the raw `base_url`.** `ProviderConfig` and `ModelEndpoint` now redact URL credentials/query fragments, but the adapter's custom `Debug` directly prints `self.config.base_url`. Userinfo is rejected, which reduces severity, but query strings can still contain routing tokens or signed parameters. Delegate to the config's redacted formatter or use the same URL-redaction helper.

**P2 — cancelled/timed-out DNS resolution can leave detached resolver threads.** The relay correctly delegates DNS to the OS now, but it wraps blocking `ToSocketAddrs` in a newly spawned thread. Cancellation stops waiting for it; it cannot stop the underlying resolver call. A pathological resolver plus repeated requests can therefore accumulate blocked threads. A bounded resolver worker/pool would contain this.

**P2 — safe serialization is intentionally lossy.** MCP environment/header values and redaction literals are now redacted by the ordinary `Serialize` implementation. That's secure, but it means `RuntimeConfig` serialization is effectively a **safe-export representation**, not a faithful operational round trip when those fields are populated. I would eventually distinguish the APIs explicitly—e.g. `to_redacted_json()` versus an intentionally controlled persistence mechanism—so library consumers don't assume standard serde round-trip semantics.

**P2 — stale crash artifacts can accumulate.** Orphan `.part-*` blobs and prepared-but-never-committed checkpoint files are bounded indirectly but are not aggressively reclaimed. Recovery/retention could safely clean artifacts that no durable record references.

One earlier concern does **not** need escalation: although `exec` accepts a model-provided `timeout_ms`, the registry supplies an independent `ToolExecutionContext` deadline based on the operator's `shell_timeout_ms`. So the model cannot extend actual execution beyond the outer policy deadline.

# Round 6 verdict

**Fix required, but narrowly.**

I find **2 P1s, no P0s**. Unlike the earlier rounds, both stem from essentially the same remaining design boundary:

> **the exact model-visible message and its surrounding lifecycle are not yet one crash-recoverable transaction.**

I would tackle them together rather than as two patches:

1. Introduce a durable `emit_message` transaction with an inline/blob recovery reference.
2. Add recovery for assistant tool calls that never reached `ToolRequested`.
3. Build the deterministic failpoint harness around that transaction.
4. Run the complete tool-call lifecycle under every interruption point.

If that work passes cleanly, **Round 7 can probably be a release-readiness audit rather than another architecture hunt**. At this stage I would expect the threshold for “no remaining highly concerning issues” to be realistically within reach.
