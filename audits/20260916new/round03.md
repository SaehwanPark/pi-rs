# Round 3 audit

I audited current `main` at `a0662b15b44f5e101327ccef9582f3a2985aae3f`. The four Round 2 fixes are genuinely present, and I would close them **as originally reported**. The provider adapter no longer silently reposts inside one attempt, `BudgetExhausted` is now a real terminal status and the one-shot runner treats it as failure, `with_failover()` preserves the session capability requirement, and resume now restores model/context epochs rather than blindly restarting at primary/epoch 0.

The deeper Round 3 pass does, however, uncover another set of **high-concern issues**. These are concentrated around process semantics, crash consistency, and ownership of durable state rather than the broad boundary problems from Round 1.

| Round 2 item                              | Round 3 disposition                                                                                                                                                |
| ----------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Duplicate hidden provider POSTs           | **Closed as reported.** One adapter attempt now corresponds to one POST, with an explicit regression test.                                                         |
| Resume loses model/context epochs         | **Closed at normal clean-shutdown level.** The new `ResumeState` is a substantial improvement; crash consistency around that projection remains a new issue below. |
| Budget exhaustion reported successful     | **Closed.** Dedicated `BudgetExhausted` state and one-shot failure handling are present.                                                                           |
| `with_failover()` weakens capability gate | **Closed.** Required capabilities survive policy replacement and have a separate explicit override API.                                                            |

## P1-1 — `exec` kills verbose commands and then reports them as successful

This is the most immediately actionable problem I found.

The output capture ceiling defaults to eight times the model-visible output budget. With the default 8 KiB tool-output setting, that means an `exec` starts truncating around **64 KiB of process output**.

When the output budget is reached, `drain()` does not merely stop retaining output. It breaks out and calls `terminate_child_tree(child)`.

The later completion logic then does this:

```rust
if drained.truncated {
  let mut outcome = ToolOutcome::succeeded(text);
  outcome.reduced = true;
  return Ok(with_elapsed(outcome.with_status(code), elapsed));
}
```

So the harness intentionally kills a potentially mutating process halfway through execution and then records `Succeeded`, irrespective of why that process exited.

That directly contradicts the module's otherwise excellent rule that killing a command mid-flight means completion is unknown. The existing large-output test only checks that output became truncated/reduced; it never checks that the command actually reached its end or that the lifecycle state is truthful.

A realistic failure is a build/deploy script that emits 64 KiB of logs before reaching its state-changing final steps. pi-rs can kill it at the log threshold and tell the model it succeeded.

The preferred fix is **not to terminate the process because capture is full**. Once the capture limit is reached, stop retaining and forwarding additional bytes, but continue draining stdout/stderr into a discard sink so pipes cannot block, then wait for the real exit status. Memory remains bounded and the command is allowed to complete. If you intentionally choose a policy where excessive output terminates execution, then the result must be `Unknown`, never `Succeeded`.

A regression test should run a command that emits well over the capture ceiling and only afterward creates a sentinel file. A successful outcome must imply the sentinel exists. Also test a verbose command that eventually exits nonzero; truncation must not turn that into success.

## P1-2 — The canonical trace and resume projection are still not crash-atomic

The Round 2 resume fix adds a semantic projection for model epochs and compactions, but it now makes an existing durability issue more important.

`StoreTrace::emit()` explicitly writes the canonical trace event **first**, and only afterward appends the corresponding semantic `SessionRecord`. Message persistence is another separate call.

For example, the sequence can be:

```text
trace.jsonl: UserMessage committed
             ↓ crash here
session.jsonl: corresponding SessionMessage not written
```

or:

```text
trace.jsonl: ModelEpochStarted committed
             ↓ crash here
session.jsonl: Epoch projection not written
```

The journal deliberately writes almost every state-changing event through immediately, specifically so a crash cannot lose transitions.  But `Store::restore()` reconstructs continuation state from `session.jsonl`; it consults the trace tail only to take the maximum sequence number. It does not repair or reject a semantic projection that trails the canonical trace.

That means a crash in exactly the wrong few instructions can leave the authoritative trace saying one thing and `--resume` quietly continuing from another.

There are several important variants. A streamed assistant answer can be visible to the user and durably flushed through `ModelRequestCompleted`, then disappear from resumed context if the separate semantic-message append never happens. A model epoch can exist in the trace but not in the resume projection. A compaction completion can exist canonically but fail to reach the projected compaction record.

Checkpoint creation has the inverse ordering in part of its lifecycle: capsule/barrier storage precedes the later `CheckpointCreated` trace event. So there are crash windows in both directions.

I would avoid trying to patch each individual window. Establish an explicit transaction/recovery contract. The clean designs are either a **single authoritative append record carrying both event and semantic projection**, from which trace/session views are derived, or a small WAL/projection-watermark scheme where every semantic projection says through which canonical sequence it is complete.

On resume, compare the projection watermark to the canonical durable sequence **before any provider contact**. If the projection trails, deterministically replay the missing canonical events into the projection. If exact reconstruction is impossible, fail closed rather than silently shortening history.

One important implication is that the trace needs enough data to reconstruct model-visible state. For instance, ordinary `ToolCompleted` contains status, sizes, and an optional recovery blob but not the normal visible tool-result text.  If trace is to remain the recovery authority, either that payload needs a durable reference or the whole tool terminal event + semantic message needs one atomic/WAL transaction.

I would add failpoint tests at every trace→projection boundary: kill after the first durable write for user messages, assistant messages, model epochs, compactions, checkpoints, and tool results, then reopen and prove either exact repair or explicit refusal.

## P1-3 — Crash recovery does not reconcile an in-flight mutating tool before resume

The code already has nearly all the pieces for this, which makes the omission notable.

Before tool code executes, pi-rs durably emits `ToolStarted`; the registry intentionally calls the start observer after approval/preflight but before executing the tool.

Consider:

```text
ToolRequested
ToolStarted
filesystem/database/network side effect happens
             ↓ process dies
(no ToolCompleted / ToolUnknown)
```

The assistant's tool-call message may already be in semantic history, and the environment may be partly or fully mutated.

The new normal resume path reconstructs messages, epochs, checkpoint state, and compaction state. It does **not** scan the canonical tool lifecycle for a `ToolStarted` lacking a terminal event.

Interestingly, the replay subsystem already knows the right rule: a mutating historical call ending in `Started` or `Unknown` is classified as `ReconcileBeforeReplay`.  Normal `--resume` should uphold the same invariant.

Before the first resumed model request, recovery should scan from the last reliably closed boundary and detect unmatched tool lifecycles. Reconstruct the `ToolRequest` from `ToolRequested`. For a started mutating call, invoke that tool's reconciliation logic. `Committed` and provably `Unmodified` can be normalized into a durable recovery result; ambiguous/diverged/manual-inspection outcomes should block automated continuation. A read-only call can be safely rerun or explicitly closed as interrupted.

There is also a protocol issue today: an assistant tool call with no corresponding tool-result message can be fed into the resumed provider history alongside a new user message. Some OpenAI-compatible servers will reject that history even before the side-effect uncertainty becomes relevant.

A strong test should deliberately terminate execution **after the real mutation but before terminal recording**, then resume. Assert that no provider request occurs until reconciliation has happened and that uncertain mutation is never replayed automatically.

## P1-4 — L0 history eviction is still not part of resumable state

Round 2 correctly added a semantic `Compaction` projection for L1/L2/L3 context changes. The session schema now explicitly says epoch and compaction projections let resume restore reduced context.

But L0 eviction takes a separate path.

`evict_oldest()` emits `ContextReduced`, then physically drains oldest messages from `self.messages`. It deliberately does **not** advance the semantic compaction epoch.

There is no corresponding reduction/eviction variant in `SessionRecord`; its durable variants are header, message, epoch, compaction, and checkpoint barrier.  Consequently, `SessionLog::restore()` knows how to apply summary compactions and checkpoint barriers, but nothing tells it to remove messages previously lost through L0 eviction.

This is especially relevant during failover to a backup with a smaller context window: `rebudget()` can deliberately evict old history so the backup can operate. On a later `--resume`, the active backup epoch is now faithfully restored—but the history it was running with can be silently re-expanded.

That can change model behavior, undo a context-safety decision, or immediately put the resumed backup back over its context limit.

Add a durable `SessionReductionRecord`/`ContextEviction` projection containing the exact model-visible boundary, preferably canonical message/event identities rather than just byte counts. Then a resume after:

```text
primary → narrow backup → L0 eviction → close → resume
```

should generate exactly the same model-visible message sequence that existed immediately before close.

I would also tighten the canonical `ContextCompactionEpoch` range semantics while working here. Its schema says the sequence range is what the summary **replaces**, but the runtime currently derives the upper bound from the latest cited event, which can include messages deliberately retained verbatim. The new session projection compensates using `retained_messages`, but a canonical trace consumer should not need a second representation to correct an over-broad "replaces" claim.

## P1-5 — Blocking HTTP semantics are still inconsistent with cancellation and configured deadlines

This actually appears in two subsystems.

For model providers, `read_timeout_ms` is documented as the requested idle budget and defaults to **300,000 ms**. But the underlying `ureq::Agent` clamps each socket read to at most **2,000 ms** for cancellation polling.

After the Round 2 duplicate-POST fix, a timeout while waiting for response headers now correctly does **not** repost—but the request simply fails after that short socket timeout.  The new regression test explicitly waits 2.5 seconds before sending response headers and expects a timeout.

That means the nominal 300-second logical timeout is effectively about two seconds **before headers**.

After headers the opposite occurs. `read_stream()` catches every transient two-second read timeout and loops indefinitely, with no logical idle deadline based on `read_timeout_ms`.  A server that sends HTTP headers and then remains silent forever can therefore remain alive forever unless the user explicitly cancels.

So the same setting currently behaves approximately as:

```text
before response headers: ~2 seconds
after response headers:  potentially unbounded
documented idle budget:  300 seconds
```

The normal `RuntimeConfig` model endpoint doesn't expose those timeout fields either, so CLI users cannot correct this asymmetry themselves.

HTTP MCP has the companion problem. `McpTransport::call_with_context` has a default implementation that ignores its `ToolExecutionContext`; stdio overrides it, but `HttpTransport` does not. HTTP MCP instead relies on fixed 60-second `ureq` timeouts.  A user can therefore cancel a mutating HTTP MCP tool and still wait up to the blocking network timeout before the runtime regains control, despite the Round 1 cancellation architecture.

I would solve both through the same underlying direction: distinguish a short **poll/cancellation interval** from a real **logical idle deadline**, and use an HTTP implementation where cancellation can interrupt one in-flight request without issuing a replacement request. A lazily initialized async transport is a reasonable tradeoff here; it need not affect the startup path until an HTTP provider/MCP endpoint is actually used.

Provider tests should cover "headers arrive after 2.5 seconds but before a 5-second logical timeout → success", "headers arrive then stream stays silent past logical timeout → Timeout", and prompt cancellation. HTTP MCP should get the analogous hanging-server test, asserting prompt return and `Unknown` for a mutating call whose remote completion cannot be established.

## P1-6 — There is no exclusive session ownership/lease

`Store::resume()` opens the existing semantic log and trace journal for append without obtaining any exclusive session lock. `TraceJournal::open()` recovers the current maximum sequence into process-local memory and then calculates later sequence numbers from that local value.

Two processes can therefore resume the same session concurrently:

```text
process A reads last_seq = 100
process B reads last_seq = 100

A writes seq 101
B writes seq 101
...
```

Even if filesystem append operations themselves remain intact, canonical sequence uniqueness is lost and two independent conversational branches become interleaved into one supposed linear session.

The same missing ownership signal affects retention. Starting a new non-resumed session invokes retention with `keep_newest = 1`.  Retention explicitly does not track open handles; it plans victims by identifier age/size and then removes their files/directories.

Thus, under concurrent pi-rs processes sharing a `state_dir`, an older-but-still-active session can become a retention victim once another newer session exists. On Unix, an already-open file descriptor may continue writing to an unlinked file, making the active process appear healthy while its durable path has vanished. On Windows, deletion/open-handle behavior can instead surface as failures. Neither outcome is acceptable for a durable harness.

I would introduce a per-session **exclusive lease** held for the `Session` lifetime. `begin`/`resume` acquire it; a second resume fails cleanly with "session is active elsewhere." Retention takes a nonblocking lease before selecting/deleting a victim and skips leased sessions. A separate short store-level lock should serialize simultaneous retention passes.

This one architectural primitive solves both concurrent-resume corruption and retention-vs-live-session deletion.

## Additional P2 findings

I would not hold Round 3 open on these alone, but they are worth fixing after the P1 set.

The dynamic `ToolRegistry` has a check/use race: it reads one tool's metadata/schema, releases the lock, performs approval, emits the execution-start boundary, then reacquires the map and executes whatever object is now registered under that name using `expect("tool exists")`. Concurrent unregister can panic; concurrent replacement can execute a different implementation than the one whose risk metadata was approved.   Store `Arc<dyn Tool>` values and clone one stable tool instance at dispatch start.

MCP advertises `roots` support during initialization, but the transport is response-oriented and has no inbound JSON-RPC request dispatcher for a server calling `roots/list`.  Until inbound requests are supported, advertise `roots: None`; otherwise compatible servers are entitled to invoke a feature the client cannot fulfill.

`Runtime::with_policy()` still silently ignores an invalid `policy.cwd` because `Workspace::new(cwd)` is wrapped in `if let Ok(...)`; the existing workspace remains active. The CLI currently neutralizes this path by making `--cwd` authoritative, so I classify it P2, but the library API should become fallible rather than silently executing in the wrong directory.

Finally, the documented "pooled agent per timeout profile" does not actually populate its `OnceLock<BTreeMap<...>>`; a cache miss builds an agent without inserting it. That's primarily a performance/implementation mismatch rather than a correctness problem.

## Round 3 verdict

**Fix required. I would not end the audit loop yet.** I find **six P1/high-concern areas and no P0/critical defect**.

The implementation order I would use is:

1. Fix `exec` truncation first; it is small, concrete, and can currently certify interrupted mutation as success.
2. Establish a crash-consistency/recovery transaction model for trace + semantic projection, including in-flight tool reconciliation.
3. Make every form of model-visible reduction resumable, including L0 eviction.
4. Repair provider and HTTP-MCP timeout/cancellation semantics, preferably through one cancellable HTTP abstraction.
5. Add exclusive session leases and make retention lease-aware.
6. Then harden the dynamic tool registry and MCP capability advertisement.

Round 2 did meaningfully improve the design; the new problems are not evidence that those fixes were ineffective. They are what becomes visible once the obvious boundary failures are removed and the audit moves into **"what happens if the process dies at any instruction?"** and **"what happens if two harnesses touch the same state?"** territory.

For Round 4, after these fixes, I would specifically inject failures at durable write boundaries and concurrent lifecycle transitions rather than mainly reading happy-path code. That should tell us whether the project is approaching the point where no highly concerning issues remain.

## Closure addendum

The follow-up hardening pass addressed the findings above and the subsequent durability review:

- `exec` now drains after capture truncation and reports cancellation/deadline completion as unknown.
- Trace/session projection writes use a bounded WAL with recovery validation, durable L0 reduction records, checkpoint epoch joins, and strict tool reconciliation before provider contact.
- Provider and HTTP-MCP cancellation workers are joined; relay sockets use a per-attempt nonce and preserve configured HTTP proxy routing.
- Session leases protect concurrent resume and retention; dynamic tool replacement uses stable `Arc` instances and risk metadata is reconciled.
- Context-reduction/checkpoint payloads, JSONL lines, WAL size/count, capsule files, and checkpoint aggregates are bounded and fail closed.

Verified after the final edits: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, and `git diff --check` all pass. The audit remains conservative for any state that cannot be reconstructed exactly: it refuses continuation rather than contacting a provider or replaying uncertain side effects.
