# Round 7 audit

I audited current `main` at **`e41b99c78af92093eee464d9fb74dc6d57a8f97f`**, including merged PR #97. The PR-head CI is green on Ubuntu, macOS, and Windows across fmt, clippy, tests, docs, and mdBook.

The Round 6 work is successful: I consider **both Round 6 P1s closed**. The new exact-message recovery transaction is the right architectural fix, and recovery of assistant tool calls that never reached `ToolRequested` is conservative and does not replay tools.

The release-readiness pass does, however, uncover issues mostly outside that patch.

**Round 7 verdict: fix required. No P0; four P1/high concerns.**

## Round 6 disposition

| Round 6 finding                                                  | Round 7 status                                                                                                                                                        |
| ---------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Message-bearing canonical event can outlive its semantic message | **Closed.** Exact redacted message recovery is prepared before canonical append, inline or blob-backed, with failpoints around prepare/canonical/semantic boundaries. |
| Assistant tool call can be durable before `ToolRequested`        | **Closed.** Resume now converts provably unexecuted calls into explicit `ToolRequested → ToolFailed → ToolResult` lifecycles without executing them.                  |

The new design is considerably better than the previous event-specific reconstruction switch.

---

## P1-1 — Tool-call IDs are treated as session-global during recovery, but live execution does not enforce that invariant

This is the most important newly discovered persistence mismatch.

`ToolCallId` is documented as identifying **one tool invocation**, but the OpenAI-compatible decoder adopts the provider's ID verbatim:

```rust
ToolCallId::from_string(id)
```

There is no check that this ID has never appeared earlier in the session.

Live execution also accepts repeated IDs. So a local/OpenAI-compatible model can legitimately produce something like:

```text
turn 1 → call_1 → read(...)
turn 2 → call_1 → grep(...)
```

and both turns can run and close normally.

Recovery is different. Multiple paths use session-global maps/sets keyed only by `ToolCallId`:

```rust
BTreeSet<ToolCallId>
BTreeMap<ToolCallId, ...>
```

and explicitly reject a second occurrence as:

```text
duplicate tool request
assistant tool call ... appears more than once
```

Therefore:

```text
live session succeeds
→ session closes
→ next --resume
→ recovery rejects history as corrupt
```

This is especially relevant for local/experimental OpenAI-compatible models, which may generate simple call IDs such as `call_0` repeatedly.

There is an even worse variant within a single assistant response: the decoder does not reject two completed calls with the same provider ID before tool execution. That makes tool-result correlation intrinsically ambiguous.

### Recommended fix

I would stop using the provider's `tool_call_id` as the durable lifecycle primary key.

A robust causal structure is:

```text
assistant completion EventId
    ↓ parent
ToolRequested EventId
    ↓ parent
ToolStarted EventId
    ↓ parent
ToolCompleted / ToolFailed / ToolUnknown
```

Keep the provider's `ToolCallId` as the protocol correlation label, but identify the actual invocation durably by the `ToolRequested` event identity.

Concretely:

* reject duplicate call IDs **within one assistant response** before executing anything;
* parent each normal `ToolRequested` to the assistant message/completion event, just as recovery-generated requests already do;
* parent `ToolStarted` to its `ToolRequested`;
* parent terminal tool events to the start/request;
* key recovery state by that causal invocation identity rather than globally by provider call ID;
* retain a compatibility path for older/imported traces lacking parents.

Regression tests should deliberately reuse `"call_1"` across two turns and across two model rounds within one user turn. Both valid cases should survive close/resume.

---

## P1-2 — Normal interactive exit never writes `SessionEnded`

This is a direct violation of the lifecycle contract the runtime now relies on.

`TurnLoop::end_session()` explicitly says:

> An explicit close is what distinguishes “the user finished” from “the process died”.

The one-shot `pi-rs run` path honors this by calling:

```rust
session.close()
```

But the interactive composition does not.

`interactive::execute()` essentially does:

```rust
run::open_session(..., |session| {
  ...
  Loop::new(...).run(session)
})
```

and there are **zero calls to `session.close()` or `close_after_failure()` anywhere in `interactive.rs`.**

Inside the loop:

```rust
InterruptAction::Quit => return Ok(())
```

and `/quit` likewise returns successfully.

Thus:

```text
user enters /quit
→ Loop::run returns Ok
→ SessionHandle drops
→ lease is released
→ no SessionEnded(UserExit)
```

The help text literally says:

```text
/quit, /exit  end the session
```

but durably they do not.

Since `SessionSummary.closed` is derived from the latest `SessionStarted`/`SessionEnded`, cleanly exited interactive sessions remain indistinguishable from abruptly killed sessions.

The failure path has the corresponding issue. A recoverable provider/turn failure ends the interactive loop after converting the typed error to `String`; unlike one-shot execution, it never calls `close_after_failure()`, so there is no `SessionEnded(Fatal { ... })`.

### Recommended fix

Make the interactive loop return a typed exit reason rather than `Result<(), String>`, for example:

```text
UserQuit
TurnFailure(TurnError)
SurfaceFailure(...)
```

Then the composition root owns closure:

```text
UserQuit
  → session.close()
  → SessionEnded(UserExit)

recoverable TurnFailure
  → session.close_after_failure(error)
  → SessionEnded(Fatal)

TurnError::Sink
  → DO NOT write a SessionEnded
  → drop and require recovery next open
```

That centralizes the exact distinction the durable format is designed to preserve.

Add process-level tests for `/quit`, idle Ctrl-C, provider failure, and sink failure. Assert the latest lifecycle event and `SessionSummary.closed`.

---

## P1-3 — Interactive management commands can swallow a durable sink failure and keep using the session

This is related to P1-2, but it is a separate safety problem.

For normal user turns, interactive mode correctly stops on a `TurnError::Sink`.

But slash commands do not.

For example `/compact` does:

```rust
match session.compact(...) {
  ...
  Err(error) => {
    self.write_note(&[format!("compaction failed: {error:?}")])?;
  }
}
Ok(...)
```

and then the interactive loop accepts another prompt.

`/compact-phase` does the same.

`SessionHandle::failover_manual()` and `switch_back_manual()` are worse for classification because they turn the runtime's `TurnError` into an ordinary `String`; interactive mode prints `"failover refused"` and continues regardless of whether the actual problem was a harmless policy refusal or a failed durable write.

That contradicts the existing `TurnError::session_recoverable()` contract:

> a sink failure says no: the runtime could not record what it was doing, so continuing would build on history it cannot trust.

There is a concrete dangerous sequence in manual failover:

```text
emit ModelEpochStarted(backup)
  WAL prepare succeeds
  canonical event succeeds
  semantic epoch projection fails
        ↓
interactive prints "failover refused"
runtime does NOT push backup epoch in memory
        ↓
user submits another turn
runtime continues generating as old primary
```

Recovery later sees the durable backup epoch and can reconstruct a different active-model timeline from the one the live process continued using.

Compaction is somewhat safer because live message replacement is deliberately deferred until all durable events succeed, but a failed compaction still leaves recovery/WAL state that should be reopened rather than ignored.

### Recommended fix

The interactive surface must preserve error typing.

A useful split would be:

```text
CommandError::Refused(...)
CommandError::Runtime(TurnError)
```

Policy conditions such as:

```text
no backup configured
already on backup
hard capability gap
```

should be `Refused`, **not `TurnError::Sink`**.

Actual durable sink failures should immediately poison/end the current interactive handle:

```text
TurnError::Sink
→ leave process/session loop
→ don't emit SessionEnded
→ next open runs recovery
```

For `/compact` and `/compact-phase`, ordinary "nothing to do" already returns `Ok(0)`, so propagating actual errors is straightforward.

---

## P1-4 — In-flight Ctrl-C cancellation is effectively unimplemented on Windows

The interactive design intentionally disables raw mode while a turn runs. Ctrl-C then arrives as an OS signal rather than as a crossterm key.

On Unix, pi-rs handles that correctly: `TurnInterruptGuard` temporarily installs a SIGINT handler that flips the `CancelToken`.

On non-Unix platforms, including Windows, the implementation is:

```rust
pub struct TurnInterruptGuard;

impl TurnInterruptGuard {
  pub fn install(_cancel: &CancelToken) -> Self {
    Self
  }
}
```

So it does nothing.

Under normal Windows console behavior, Ctrl-C is delivered as `CTRL_C_EVENT`; each console process initially has a default control handler that calls `ExitProcess`. Applications must install a handler with `SetConsoleCtrlHandler` if they want to consume Ctrl-C themselves. ([Microsoft Learn][1])

Consequently the advertised interaction:

```text
Ctrl-C during generation
→ cancel turn
→ keep process/session/draft alive
```

works on Unix but, on an ordinary Windows console, can instead become:

```text
Ctrl-C
→ default Windows handler
→ process termination
```

That is particularly undesirable while a mutating tool is running: the durability layer may recover afterward, but a controlled cancellation has been replaced by an abrupt process death.

### Recommended fix

Implement a Windows `TurnInterruptGuard` using `SetConsoleCtrlHandler`.

The handler should do almost nothing:

```text
CTRL_C_EVENT + active turn
→ atomic store(true) into cancellation flag
→ return TRUE
```

Do not allocate, log, acquire locks, or perform filesystem work in the control handler. Microsoft documents that Windows invokes these handlers on a newly created thread and that returning `TRUE` marks the event handled. ([Microsoft Learn][2])

Restore/remove the handler when the guard drops.

Given that Windows is now part of the CI matrix, I would also add a Windows-specific child-process console test if practical. At minimum, directly exercise the handler and cancellation state; preferably verify that the process survives an actual control event and the turn becomes `Cancelled`.

---

# P2 / release hardening

The remaining lower-priority items are now fairly bounded.

**Oversized semantic messages can still create an unrecoverable transaction.** `SessionLog` has a 16 MiB line limit, but `Session::emit_message()` does not preflight the final `SessionRecord` before WAL preparation and canonical append. A message over that bound can therefore produce a durable canonical event and recovery payload, then fail every attempt to append its semantic projection. Preflight the serialized redacted semantic record before changing durable state, or externalize oversized semantic messages.

**`panic = "abort"` contradicts the terminal cleanup guarantee.** `RawTerminal` says panic unwinding passes through `Drop` and restores raw mode, but release builds explicitly use `panic = "abort"`. A release panic while the editor owns raw mode can leave the terminal misconfigured. Either install a minimal panic hook that restores the terminal before aborting or reconsider abort semantics for the CLI binary.

**`OpenAiCompat::Debug` still prints the raw `base_url`.** `ProviderConfig` now properly redacts URL queries/fragments, but the adapter's custom `Debug` uses `self.config.base_url` directly. Userinfo is rejected, but signed/query-token URLs can still leak. Reuse the redacted config representation.

**Cancelled DNS lookups can leave detached resolver threads.** `resolve_target()` correctly uses the OS resolver now, but spawns one thread per lookup because `ToSocketAddrs` is blocking. Cancellation stops waiting, not resolving. A bounded resolver worker/pool would prevent pathological resolver failures from accumulating threads.

I would also clean up orphaned message-recovery blobs created before a transaction's WAL prepare and correct the workspace package metadata's repository URL (`saehwan/pi-rs` versus the actual `SaehwanPark/pi-rs`) before publishing packages, but neither rises to P1.

# Round 7 verdict

The core recovery architecture has crossed an important threshold: **I no longer see a fundamental WAL/message/compaction design flaw.** PR #97 closes the last major crash-atomic message gap.

The remaining high-concern issues are now primarily **composition and lifecycle invariants** rather than storage architecture:

1. make tool invocation identity causal rather than session-global provider-ID based;
2. make interactive clean/fatal exits actually close the durable lifecycle;
3. never continue an interactive handle after a real sink failure;
4. implement the advertised in-turn Ctrl-C semantics on Windows.

After those fixes, I would make **Round 8 the final acceptance pass**: regression-test those four issues, inspect the release profile/package metadata once more, and specifically look for any remaining path where live execution accepts a state that `--resume` later rejects. If those checks come back clean, I expect to be comfortable saying there are **no remaining highly concerning issues**.

[1]: https://learn.microsoft.com/en-us/windows/console/generateconsolectrlevent?utm_source=chatgpt.com "GenerateConsoleCtrlEvent function - Windows Console | Microsoft Learn"
[2]: https://learn.microsoft.com/en-us/windows/console/registering-a-control-handler-function?utm_source=chatgpt.com "Registering a Control Handler Function - Windows Console | Microsoft Learn"
