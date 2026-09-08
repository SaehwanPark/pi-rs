# Implementation Status

Working status for the `pi-rs` runtime. Roadmap intent lives in [`ROADMAP.md`](../ROADMAP.md);
this document records **what exists, what is proven, and what is deliberately not done yet**.

Verification for everything marked *done* below:

```
cargo test --workspace      # 341 tests
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
| Native tools | `pi-rs-tools` | done | 88 |
| Turn loop + recovery | `pi-rs-runtime` | done | 29 |
| Surface | `pi-rs-tui` | not started | – |
| Composition root | `pi-rs` binary | one-shot command done | 12 |

## What the one-shot command added

`pi-rs run --config <file> --cwd <workspace> --prompt <text>` composes the
configured primary `OpenAiCompat` provider, profile context policy,
workspace-confined built-ins, durable store session, and `TurnLoop`. It streams
assistant text to stdout and sends provenance-labeled reasoning, tool activity,
diagnostics, and failures to stderr. Input and endpoint validation complete
before the first provider request, and mutating tools stay denied unless
`auto_approve_mutating` is explicitly true.

`StoreTrace` is the runtime/store bridge: the store stamps the sole durable event
sequence, and `AttributedMessage` binds semantic session messages to their real
introducing event, model, and epoch. The canonical trace and resumable session
projection remain separate files.

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

## Benchmarks

`bench/startup.sh` measures process startup of the release binary: one cold exec, then warm
min/mean/median/max. `bench/render.sh` measures the renderer and the command parser per event
and per line -- width 80 monochrome, width 80 colour, width 20 colour (the wrap-dominated case),
and `Input::parse` over representative command lines -- and exits non-zero when a case exceeds
its budget.

Budgets are deliberately not run in CI. Each is about five times a baseline recorded on one
machine, which is generous locally and meaningless on a shared runner, where identical code
measures several times slower for reasons nobody changed. They are a pre-merge gate for changes
to rendering, streaming, or the startup path, and the numbers they were derived from are recorded
beside them.

## Not done yet

1. `pi-rs-tui` — ratatui transcript rendering.
2. Interactive binary composition root/TUI (the headless one-shot path is
    complete; `pi-rs-tui::editor` holds the buffer, its rows, and its recall, but
    nothing yet feeds it terminal events).
3. End-to-end agent loop against the real local endpoint (the one-shot tool loop is
   covered against a fake OpenAI server; single completions are verified live;
   tool round-trips through the real provider are not).
4. Startup benchmarks with full composition root (`bench/startup.sh` harness implemented).
5. Phase 6 Pi compatibility fixtures.
6. Surface write errors are not yet routed through the runtime's fallible event
   channel; stdout/stderr write failures remain a deferred interactive-surface
   concern rather than being silently reclassified as model or storage failures.
7. CLI-level fault injection after a durable session is opened is deferred; the
   runtime sink-failure seam directly proves cancellation and terminal failure.
8. Approved `exec` is intentionally not an OS sandbox, and outward-pointing
   symlinks require operating-system isolation if they are in scope. Windows CI
   coverage also remains deferred; current CI targets Ubuntu and macOS.
9. A temporal terminal-streaming benchmark is deferred beyond deterministic
   multi-chunk ordering tests.

## Real-endpoint verification

Verified in the provider slice against `http://127.0.0.1:8080/v1` (llama.cpp,
model `qwen3.8-flash`, `reasoning_content` exposed):

* SSE parsing, `reasoning_content` → `Native` provenance, finish reason `stop`.
* Tool schema serialization and request body shape.

The runtime's tool loop is covered by scripted-provider unit tests and the one-shot
command's fake OpenAI server integration test. It has not yet been exercised against a
live model.
