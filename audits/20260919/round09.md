# Round 9 audit — sign-off

I audited current `main` at **`f07f363e4ea591486f1a92c9b0168d68842e182f`**, the merge of PR #99.

**Verdict: pass. I found no P0 or P1 issues.** At this point I agree that **no highly concerning issue remains from this audit cycle**.

## Round 8 findings

| Finding                                                             | Round 9 status |
| ------------------------------------------------------------------- | -------------- |
| P1: decoded-but-unexecuted `ToolFailed` left an open projection WAL | **Closed**     |
| P2: MCP resolver spawned unbounded detached DNS threads             | **Closed**     |

The P1 fix is the right one. `record_unexecuted_calls()` now uses a dedicated parent-aware no-message path:

```rust
self.emit_without_message_with_parent(
  Some(turn_id.clone()),
  AgentEvent::ToolFailed(...),
  Some(requested.meta.event_id),
)?;
```

That preserves both invariants simultaneously: the durable causal chain remains `ModelRequestCompleted → ToolRequested → ToolFailed`, while `ToolFailed` is explicitly terminal evidence rather than a model-visible `ToolResult`.

More importantly, the new regression test is at the correct abstraction level. It drives:

```text
decoded ToolCall
→ provider stream failure
→ record unexecuted ToolRequested/ToolFailed
→ StoreTrace
→ session.finish()
→ Store::restore()
```

and verifies that the original `ContextOverflow` remains the turn outcome, the WAL closes, immediate restore succeeds, no interrupted tool remains, no synthetic tool-result message enters resumed context, and the terminal event is parented to its exact request. That is substantially stronger than the earlier in-memory `Recorder` test.

## MCP resolver

The MCP HTTP relay now mirrors the provider relay's bounded resolution policy: **one process-wide resolver worker plus one queued job**. If platform DNS itself wedges, cancellation/deadlines can abandon their caller while resource growth stays bounded; later requests fail closed rather than creating unlimited resolver threads.

This is the right safety tradeoff for the current blocking resolver architecture.

## Verification

PR #99's hosted CI run **35465786687** completed successfully on:

* Ubuntu: fmt, clippy, full workspace tests, docs, mdBook, startup benchmark.
* macOS: same, including startup benchmark.
* Windows: fmt, clippy, full workspace tests, docs, mdBook.

I also checked the affected runtime/store boundary beyond the diff. Multi-call failed responses remain safely scoped by individual request `EventId`s; partial lifecycle persistence continues to recover conservatively; and the fix does not accidentally add failed unexecuted calls to model-visible history.

## Remaining non-blocking hardening

I would leave a few items on the backlog, but none justify another mandatory audit/fix cycle:

* **P2 maintainability:** provider and MCP relays now contain essentially duplicated bounded-resolver implementations. Factoring the networking/cancellation primitive into a small shared crate/module would prevent future policy drift.
* **P2 testing:** MCP verifies platform resolution but not resolver-queue saturation/wedged-DNS behavior directly. A resolver injection seam would make this deterministic to test.
* **P2 provenance/design:** `SessionEpochRecord` persists model/reason but not the capability snapshot. Resume reconstructs capabilities from the currently configured provider, while the canonical `ModelEpochStarted` event does retain the historical snapshot. This is defensible—current capabilities matter for current execution—but making the distinction explicit in the schema/docs would avoid ambiguity.
* **P2 integration coverage:** the interactive lifecycle still benefits from a PTY-level test covering user exit and Ctrl-C where infrastructure permits, although the underlying paths and cross-platform CI are now adequately covered.

### Overall

Across the nine rounds, the architecture has moved considerably from “works under normal execution” toward **fail-closed, crash-recoverable runtime semantics**: transactional semantic projection, interrupted mutation reconciliation, resumable reductions/checkpoints, bounded/cancellable transports, session leases, exact causal tool identities, and explicit sink-failure barriers.

I would **close the iterative high-severity audit here**. Further rounds are likely to produce diminishing returns and mostly P2/P3 hardening rather than correctness blockers.

**Round 9: accepted.**
