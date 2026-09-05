# Implementation Status

Working status for the `pi-rs` runtime. Roadmap intent lives in [`ROADMAP.md`](../ROADMAP.md);
this document records **what exists, what is proven, and what is deliberately not done yet**.

Verification for everything marked *done* below:

```
cargo test --workspace      # 320 tests
cargo clippy --workspace --all-targets --all-features   # 0 warnings
cargo fmt --all -- --check
cargo doc --workspace --no-deps      # 0 warnings
```

## Layer status

| Layer | Crate | State | Tests |
| --- | --- | --- | --- |
| Contracts | `pi-rs-core` | done | 76 |
| Durability | `pi-rs-store` | done | 66 |
| Model I/O | `pi-rs-provider` | done, verified against a real endpoint | 70 |
| Native tools | `pi-rs-tools` | done | 87 |
| Turn loop + recovery | `pi-rs-runtime` | **done in this slice** | 21 |
| Surface | `pi-rs-tui` | not started | – |
| Composition root | `pi-rs` binary | not started | – |

## What the runtime slice added

`crates/pi-rs-runtime/src/failover.rs` — the availability-failure policy.

* `FailoverPolicy::decide(kind, attempts, partial_output_emitted) -> Recovery`.
* Order is retry-then-takeover; a takeover is gated on the backup's capabilities
  against what the **active** model's epoch required, not the request's instantaneous needs.
* Explicitly not triggers: `Semantic`, `Authentication`, `ContextOverflow`, `Cancelled`.
* `partial_output_emitted` forbids retry unconditionally, whatever the retry budget says.

`crates/pi-rs-runtime/src/turn.rs` — the canonical event producer.

* Emits the documented turn shape and owns session/turn identity rather than trusting
  the store's defaults, so every event's `EventMeta` is correct at construction.
* `Trace` is the runtime's sink view (event emit + payload persistence + flush), with a
  blanket impl for `&mut T` so one trace can be shared by the request loop and tool
  adapters without moving it.
* Tools execute in declaration order, sequentially, each result appended as a
  `Role::Tool` message; output is reduced **once** — at the tool boundary — never again at
  request build.
* Cancellation is checked before the request, after it, before each tool, and on
  provider-reported cancellation. It is a status, never a fault, and never recovers.
* `TurnError::kind()` lets a caller distinguish an outage from a quality failure;
  `session_recoverable()` says whether history can still be trusted.

## Contract changes this slice required

### `CompletionUsage.certainty` (core)

`ModelProvider::stream` returns `Result<CompletionUsage, ModelFailure>`, and the `Ok` arm
could not express *"the transport ended cleanly but the model never said it was done."*
`CompletionUsage` now carries `certainty: CompletionCertainty`, defaulting to `Certain` on
deserialize so existing traces still read.

`EndedWithoutSentinel` **with** output is now `Ok(Unknown)`, not `Err(Transport)`:

* the runtime owns what an unfinished answer means. It reports a semantic failure — the
  model answered, and the answer is not usable — which is neither retried nor failed over;
* `Err(Transport)` would have invited a retry that duplicates committed output;
* `Ok(Certain)` would have accepted a half answer as final, silently.

`EndedWithoutSentinel` **without** output stays `Err(Transport)`: nothing was committed, so
retry is safe.

### `TurnLoop::requests` counts where requests happen

The request budget is spent inside `attempt`, not in the caller's round loop. Recovery
re-enters `attempt`, so a budget enforced one level up let a permanently failing provider
generate retries forever.

## Decisions worth keeping

* **`ToolRegistry::execute` takes `&self`.** The approval gate is a `DefaultGate` enum, not
  a boxed trait object, so the registry is consulted rather than mutated. The runtime holds
  `&'a ToolRegistry`.
* **A scripted failure consumes no round.** In tests, `fails(0, ..)` means "the first
  attempt fails"; the retry must re-serve the *same* round or the test asserts a different
  provider than the one under test.
* **`report.requests` counts model requests, retries and takeovers included.** From the
  caller's side a turn that keeps asking for tools and a turn that keeps retrying cost the
  same.
* **A fatal request failure still closes the turn.** The `turn_completed` event is emitted
  before `TurnError::Unavailable` is returned; a trace with no terminal event cannot tell a
  crashed session from an interrupted one.
* **`TurnError` is deliberately large.** It carries the whole normalized `ModelFailure` so
  a caller branches on `kind` without a second lookup. `clippy::result_large_err` is
  allowed at the crate root with that reason stated, not silenced per signature.
* **Token accounting mirrors the reference ratio** (3 chars/token, images at a flat 1000
  tokens) and is recorded as an estimate: `ModelRequest.needs_token_estimate` is set unless
  the provider reported real usage. `measured_input_tokens` from the last completion is
  preferred over any estimate.

## Not done yet

1. `pi-rs-tui` — ratatui transcript rendering.
2. Binary composition root — config → provider → runtime → surface.
3. End-to-end agent loop against the local endpoint (single completions are verified;
   tool round-trips through the real provider are not).
4. Startup benchmarks with full composition root (`bench/startup.sh` harness implemented).
5. Phase 6 Pi compatibility fixtures.

## Real-endpoint verification

Verified in the provider slice against `http://127.0.0.1:8080/v1` (llama.cpp,
model `qwen3.8-flash`, `reasoning_content` exposed):

* SSE parsing, `reasoning_content` → `Declared` provenance, finish reason `stop`.
* Tool schema serialization and request body shape.

The runtime's tool loop is covered by scripted-provider tests only. It has not yet been
exercised against a live model.
