# Round 2 audit

I audited the updated `main` after PR #91, merge commit `72756e2`. The remediation PR is merged and covers all six Round 1 areas; the repository now also runs its normal correctness suite on Linux, macOS, and Windows.

My assessment is that the **original Round 1 P1 findings are materially resolved as originally framed**. In particular, filesystem reads/writes are now confined by default, append is separated into a non-idempotent tool, cancellation is represented by an execution context, bounded line ingestion is centralized, and process supervision is substantially stronger. The new bounded reader is a good implementation of the ingestion requirement rather than merely an output truncation mechanism.

However, Round 2 found **four new P1/high-concern issues**. One is a regression introduced by the Round 1 cancellation work; the other three are deeper runtime/state-machine problems exposed by auditing beyond the Round 1 scope.

## Round 1 disposition

| Round 1 finding                                          | Round 2 assessment                                                                                                                                                                                        |
| -------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Symlink workspace escape                                 | **P1 closed.** Existing symlink components are checked and outside-workspace access is closed by default. Residual TOCTOU is hardening, not currently a P1 under the documented non-sandbox threat model. |
| Outside-workspace reads enabled by default               | **Closed.** Read/search/write outside flags now default false and are explicitly configurable.                                                                                                            |
| Cancellation was classification-only                     | **Mostly closed.** Tools/extensions now receive interruptible execution context. A provider-side implementation choice created new finding R2-1 below.                                                    |
| `exec` did not terminate descendants / bounded buffering | **P1 closed.** Unix uses a process group and output queues are bounded. Windows still has a lower-priority robustness issue discussed below.                                                              |
| `write append=true` reconciliation                       | **Closed.** Append is now its own non-idempotent operation and does not claim automatic reconciliation.                                                                                                   |
| Line ingestion bounded too late                          | **Closed.** `BoundedLineReader` enforces limits before materializing unbounded lines.                                                                                                                     |

So the developers' Round 1 remediation was substantive; I would not reopen those issues simply because related code appears below.

---

## P1-1. Provider cancellation polling can duplicate the same model request many times

This is the most urgent Round 2 finding.

To make provider cancellation responsive, the OpenAI-compatible HTTP agent now limits individual socket reads to at most **2 seconds**, while retaining a default logical read budget of **300 seconds**.

The problem is in the pre-response-header path. `OpenAiCompat::send()` does roughly this:

```rust
loop {
  let mut call = self.agent.post(...);

  match call.send_string(body) {
    Ok(response) => return Ok(response),

    Err(timeout) => {
      if cancel.is_cancelled() {
        ...
      }

      if logical_deadline_not_reached {
        continue;
      }
    }
  }
}
```

The `continue` does **not** continue reading the same HTTP request. It constructs another `POST` and calls `send_string(body)` again.

After a response-read timeout, the client cannot know that the original server-side generation never started. Quite the opposite: an inference server may have fully accepted the request and simply be queued, prefilling, or reasoning before it sends HTTP response headers.

Under those circumstances rupi can submit:

```text
POST generation A
  2 s with no response header
POST generation A again
  2 s
POST generation A again
...
```

This is particularly concerning for exactly the workloads rupi targets: slow local models, queued local inference servers, and cloud models with potentially expensive requests.

The test suite currently covers a quiet interval **after response headers have already arrived**, and cancellation after a stream has started. Those are good tests, but they do not exercise a server that accepts the request and delays the HTTP headers beyond the 2-second socket poll.

### Recommended correction

**Never silently retry the POST inside the provider adapter after an ambiguous transport timeout.** Once request bytes may have reached the server, exactly-once execution cannot be inferred.

The clean solution is a transport that supports cancellation of one in-flight HTTP operation, for example an async HTTP implementation where dropping/aborting the request future closes the connection without issuing another POST.

As an interim solution, one request should remain one request. A pre-header timeout should return a normalized transport failure/cancellation instead of internally resending it. The existing runtime retry policy can then make an explicit, bounded retry decision and record `ModelRetry`, which is much safer and observable.

Add an adversarial test where the fake server:

1. accepts and records a POST;
2. waits longer than 2 seconds before sending response headers;
3. counts every accepted connection/request;
4. eventually answers or observes cancellation.

The invariant should be **one provider attempt → one HTTP POST**. A second POST should occur only when the runtime emits an explicit retry event.

---

## P1-2. Session resume does not faithfully restore model/context state

This is the largest architectural issue found in Round 2.

The semantic session schema explicitly defines `Epoch` and `Compaction` records. Its own documentation says session state is the projection needed to continue without replaying the high-resolution trace.

But the normal runtime/store bridge cannot currently populate that projection completely. `Trace` exposes methods for emitting events, recording messages, payloads, and checkpoints, but no semantic model-epoch or compaction-record operation.  `StoreTrace::emit()` simply writes the canonical event to the trace, while `record_message()` projects only messages.

The store restoration code is prepared to read `SessionRecord::Epoch` and `SessionRecord::Compaction`, but merely collects them.   And the CLI's `continue_context()` then ignores those collections entirely; it restores only the latest checkpoint capsule plus messages.

This combines badly with the fresh `TurnLoop` created for every resumed process:

* model epoch starts again at `0`;
* active model starts again at the primary;
* `context_epoch` starts again at `0`;
* `session_started` starts false.

The first resumed turn then emits another:

```rust
SessionStarted {
  resumed: false,
  ...
}

ModelEpochStarted {
  epoch: 0,
  ...
}
```

into the existing journal.

But the event schema explicitly defines `resumed` as true when continuing an existing journal, and describes `SessionStarted` as establishing the initial session/epoch state.

There is a related context-compaction problem. `ContextCompactionEpoch` is specifically documented as telling replay to substitute the summary for the old canonical range, and its epoch numbering is supposed to derive from durable records so resumed processes continue monotonically.   Yet the runtime itself acknowledges that restoration currently does not consume those epoch records.

### Concrete failure cases

Suppose a session reaches:

```text
model epoch 0: primary
model epoch 1: backup
context epoch 1: compacted
context epoch 2: compacted again
```

and is then closed and resumed.

The resumed process starts internally as:

```text
model epoch 0: primary
context epoch 0
```

while the same durable trace already contains later epochs.

That affects more than bookkeeping. It can change which model answers, invalidate provenance chronology, duplicate epoch identifiers, and undo model-visible compaction. A long session that had deliberately reduced its working context may come back with the full pre-compaction message log plus summaries.

The current resume integration test catches checkpoint recovery, but it only asserts that the checkpoint capsule reaches the second provider request. It does not assert resumed lifecycle semantics, model epochs, compaction epochs, or active-model restoration.

### Recommended correction

I would treat resume as a typed state-restoration operation rather than `with_messages(...)`.

Something along these lines is preferable:

```rust
struct ResumeState {
  messages: Vec<Message>,
  active_model_epoch: ModelEpoch,
  next_model_epoch: u32,
  context_epoch: u32,
  cited_history: Option<(EventSeq, EventSeq)>,
  resumed: bool,
}
```

The durable projection should include enough information to reconstruct that state. Either:

* actually project `ModelEpochStarted` and compaction epochs into the semantic session log, then consume them during `Store::restore()`, or
* reconstruct them authoritatively from the canonical trace at resume time and reconsider whether duplicate session-record representations are needed.

For compaction, I would favor sequence-number ranges matching `ContextCompactionEpoch` rather than a separate line-index coordinate system. It makes replay/restoration line up with the canonical event address space.

Regression coverage should include failover → close → resume, compaction → close → resume, multiple compactions → resume, and checkpoint + model switch → resume. Epoch indices must remain monotonic, and the request immediately after resume should contain the same effective model-visible history that existed immediately before shutdown.

---

## P1-3. Exhausting the model-request budget is reported as successful completion

The loop correctly recognizes the pathological case:

```text
turn stopped after N model requests without a final answer
```

and sets:

```rust
report.budget_exhausted = true;
```

but then ends the turn as:

```rust
TurnStatus::Completed
```

rather than as an unsuccessful terminal state.

That contradicts the explicit meaning of `TurnReport::budget_exhausted`: the loop stopped because its budget ran out **rather than because the model produced an answer**.

This becomes particularly problematic at the CLI boundary. The one-shot `SessionHandle::turn()` discards the `TurnReport` entirely:

```rust
self.turn_with(...).map(|_| ())
```

so `budget_exhausted=true` is lost.  The outer one-shot runner sees `Ok(())`, closes normally, and reports successful execution.

For an agent harness, this is a serious correctness problem. For example:

```text
model → write file
model → run test
model → inspect error
model → edit
...
request budget reached
```

The process can exit successfully even though the model never produced its final completion or established that the task was finished.

### Recommended correction

Do not overload `Completed`.

A dedicated status is clearest:

```rust
TurnStatus::BudgetExhausted
```

or an equivalent runtime-abort reason distinct from provider failure.

Then distinguish surfaces:

* an interactive session can report budget exhaustion and remain usable for another user turn;
* `rupi run` should return a non-success exit when its sole requested turn never reached a final answer.

Add both runtime and process-level tests. The important process test is that a provider endlessly returning tool calls causes `rupi run` to exit nonzero rather than merely printing a warning to stderr.

---

## P1-4. `with_failover()` can silently weaken the backup capability gate

`TurnLoop::new()` does the right thing initially: its failover policy's required capability snapshot is derived from the actual primary model.

But `with_failover()` replaces the entire policy. It preserves an attached backup when the replacement policy does not specify one, but does **not** preserve the existing `required` capability snapshot.

That matters because:

```rust
FailoverPolicy::default()
```

requires merely:

```rust
ModelCapabilities::text_only(8_192)
```

while the policy contract says `required` represents what the session needs and the backup must be capable of the work already in flight.

A natural library call therefore has surprising semantics:

```rust
TurnLoop::new(...)
  .with_backup(&backup)
  .with_failover(
    FailoverPolicy::default()
      .with_max_attempts(3)
  );
```

The apparent intention is "change retry attempts to 3." The side effect is "forget that this session required tool calling/images/etc."

A backup lacking tools could then pass a capability gate that would otherwise correctly refuse it.

The current CLI composition does not use this setter, so immediate CLI exposure is lower than the other three P1s. But this is a public runtime API and undermines one of the project's explicit failover invariants.

### Recommended correction

Separate **policy tuning** from **session requirements**.

For example, `TurnLoop` could own the immutable required capability snapshot while a smaller `FailoverTuning` holds retry parameters. At minimum, `with_failover()` should preserve `self.failover.required` unless an explicitly named API such as `with_required_capabilities(...)` is invoked.

A regression should use a tool-capable primary and text-only backup, attach the backup, call `with_failover(FailoverPolicy::default().with_max_attempts(...))`, induce failover, and verify that the backup is still refused for the `Tools` gap.

---

## Lower-priority residual hardening

I do **not** consider these Round 2 blockers, but I would keep them on the roadmap.

On Windows, process-tree termination currently invokes `taskkill /T /F`, ignores its result, and then falls back to killing only the direct child.  A Windows Job Object would provide a stronger lifecycle guarantee, especially if `taskkill` is unavailable or denied. Given that Windows CI now exists and the primary Round 1 defect is addressed, I would classify this as P2 rather than reopening P1-4.

Likewise, the new filesystem confinement still performs validation separately from the later filesystem mutation, so it is not a capability-secure sandbox against a hostile concurrent local process. The README was correctly changed to stop claiming that it is one, and for rupi's stated guardrail threat model I consider that residual P2 hardening rather than an unresolved Round 1 vulnerability.

## Round 2 verdict

**Fix required.** I find **four P1/high-concern issues and no P0/critical issue**.

The priority I would use is:

1. **Stop duplicate provider POSTs immediately.** This can multiply model/inference consumption invisibly.
2. **Make resume state-faithful.** This is the deepest architectural correction and should address model epochs, context epochs, compaction, cited history, and resumed lifecycle together.
3. **Make request-budget exhaustion an unsuccessful turn.**
4. **Prevent failover-policy tuning from weakening capability requirements.**

The overall project is in better condition than at Round 1: those fixes were meaningful and the safety boundaries are noticeably stronger. The remaining P1s are now concentrated in **request identity and durable state-machine semantics**, rather than broad filesystem/process/protocol weaknesses.

Once these are merged, Round 3 should first adversarially verify these four cases and then move deeper into crash consistency, replay determinism, MCP lifecycle, retention, and concurrent/session-edge behavior.
