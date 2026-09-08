# Implementation Status

Working status for the `pi-rs` runtime. Roadmap intent lives in [`ROADMAP.md`](../ROADMAP.md);
this document records **what exists, what is proven, and what is deliberately not done yet**.

Verification for everything marked *done* below:

```
cargo test --workspace      # 806 tests (2026-09-08, after the full PR-series merge)
cargo clippy --workspace --all-targets --all-features   # 0 warnings
cargo fmt --all -- --check
cargo doc --workspace --no-deps      # 0 warnings
```

## Layer status

| Layer | Crate | State | Tests |
| --- | --- | --- | --- |
| Contracts | `pi-rs-core` | done | 81 |
| Durability | `pi-rs-store` | done, incl. payload bounding, externalized fields, retention | 136 |
| Model I/O | `pi-rs-provider` | done, verified against a real endpoint | 78 |
| Native tools | `pi-rs-tools` | done, lifecycle + failure/unknown outcomes | 88 |
| Turn loop + recovery | `pi-rs-runtime` | done, incl. failover epochs, cancel tokens, multi-turn handles | 41 |
| Surface | `pi-rs-tui` | done for the interactive surface: raw terminal, buffer/caret, status line, highlighting, wrap | 165 |
| Pi compatibility readers | `pi-rs-compat` | skills + prompt-template readers with fixture suites | 48 |
| Composition root | `pi-rs` binary | run, interactive, trace, skills, prompts, prompt, import-pi, export-pi, run --resume | 169 |

## What the merge series added on top of the one-shot command

* `pi-rs interactive` — one process, many turns: raw-mode terminal, editable buffer
  with wide-character-safe wrap, projected status line, semantic highlighting
  (operation vs arguments), Ctrl-C split between in-flight turn and idle draft.
* `pi-rs run --resume <id|prefix>` — continues a recorded session; id resolution is
  shared with `pi-rs trace` so two commands cannot disagree about one session id.
* `pi-rs import-pi` / `pi-rs export-pi` — Pi session files in and out, with a
  round-trip fixture and an explicit report of what could not be carried.
* Failover made readable: model epochs, takeover reasons, and the epoch that served
  the answer are durable events, not prose.
* Trace payload bounding: one journal line stays inside its inline budget; spilled
  fields are typed `externalized` records with recovery pointers.
* Benchmarks: cold start, warm start (continuing a stored session), render, and
  keystroke budgets, each with a script under `bench/`.

The compaction events are pinned in shape but still have **no producer**; pre-emptive
compaction is the current P0 (see `ROADMAP.md`, “Current priorities”).

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
min/mean/median/max. `bench/cold_start.sh`, `bench/warm_start.sh`, and `bench/keystroke.sh`
cover the cold exec, the cost of relaunching a process that continues a stored session,
and keystroke-plus-redraw latency against pinned budgets. `bench/render.sh` measures the
renderer and the command parser per event and per line -- width 80 monochrome, width 80
colour, width 20 colour (the wrap-dominated case), and `Input::parse` over representative
command lines -- and exits non-zero when a case exceeds its budget.

Budgets are deliberately not run in CI. Each is about five times a baseline recorded on one
machine, which is generous locally and meaningless on a shared runner, where identical code
measures several times slower for reasons nobody changed. They are a pre-merge gate for changes
to rendering, streaming, or the startup path, and the numbers they were derived from are recorded
beside them.

## Pi session import

`pi-rs import-pi <session.jsonl>` reads one Pi session file and reports what a pi-rs
session would hold; `--write --store <dir>` (or `--config <file>`, which also supplies
the write policy) files it as a new session. The report is stdout, the destination line
is stderr, and a dry run creates no byte on disk.

* Nothing is executed. Pi's tool calls are records of work Pi already did, mapped to
  `tool_requested` with `read_only: false`: the direction an import is allowed to be
  wrong in is the one that asks before running.
* The entry tree is honoured. The path from the newest-written entry back to the root is
  the import; entries off that path are counted per type and named, never silently folded
  in. Duplicate ids, dangling parents, and cycles are errors, not repairs.
* Pi's stored thinking becomes `ReasoningProvenance::ProviderSummary`, because pi-rs never
  streamed it. A request that stored no reasoning gets no provenance at all.
* `model_change` and `thinking_level_change` become info diagnostics rather than a model
  epoch, which would claim capabilities Pi never recorded.
* Two durable records come out of one plan: the trace journal gets the events, and the
  session message log gets the conversation, each message bound to the event that introduced
  it (a user message to its `user_message`, a reply to its first `assistant_delta`, a tool
  result to its terminal tool event). Reasoning stays trace-only, exactly as in a native
  session, so a resume sends what Pi's file showed and nothing invented. Turn ids come from
  Pi's entry tree (`turn-<entry id>`) because Pi records none, and are stable across
  re-imports.
* Tool output follows the same inline rule as a native session's: filed as a blob past the
  store's inline threshold, otherwise held by its message record. Filing every imported
  output would store the same bytes twice, and `reduced` stays false because pi-rs reduced
  nothing.
* Timestamps are parsed without a date library (`Z`, `±HH:MM`, zone-less read as UTC by
  documented convention).
* Damage is named: an unreadable line is skipped and reported as `line N: …`.

`pi-rs trace` reads an imported session back with no knowledge of Pi, which is the proof
that the import is a session rather than a transcription.

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
10. An import carries a Pi conversation into both records a session has — trace journal and
    message log — but a resumed imported session has not been exercised against a live
    provider; the resume path is proven by store-level restoration, not by a real request.

## Real-endpoint verification

Verified in the provider slice against `http://127.0.0.1:8080/v1` (llama.cpp,
model `qwen3.8-flash`, `reasoning_content` exposed):

* SSE parsing, `reasoning_content` → `Native` provenance, finish reason `stop`.
* Tool schema serialization and request body shape.

The runtime's tool loop is covered by scripted-provider unit tests and the one-shot
command's fake OpenAI server integration test. It has not yet been exercised against a
live model.

## Interactive session

`pi-rs interactive` holds one durable session in one process: raw mode, crossterm events
mapped through `pi-rs-tui::keys::intent`, the `pi-rs-tui::editor` buffer drawn with one status
line, and one turn per submit through `run::SessionHandle`, so the second turn carries the
first. Ctrl-C is intercepted before the keymap (which maps it to `Intent::Noop` on purpose):
an empty buffer quits, a non-empty buffer keeps its text.

Covered by tests: the pure decision logic (`event -> LoopAction`), frame arithmetic, the CLI
surface without a terminal. Not covered: real-terminal rendering by eye, live resize,
multi-row wrap on a terminal, and a failover that happens mid-session. Turn interruption is a
separate runtime slice; `Ctrl-C` while a turn runs is a plain SIGINT, which ends the process
without the closing flush. Bracketed paste is not enabled, so a pasted newline can submit.
