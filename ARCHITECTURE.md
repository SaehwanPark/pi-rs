# ARCHITECTURE

## 1. Purpose

This document defines the implementation-facing architecture for `rupi`.

The project should remain small at the user-facing layer while providing explicit runtime primitives for:

- provider normalization;
- session state;
- typed events;
- tools;
- context lifecycle;
- reasoning provenance;
- failover;
- interoperability;
- Pi compatibility.

The canonical rule is:

> Context is a cache, not the record.

## 2. System boundaries

```text
CLI/TUI
  |
  v
Agent Runtime
  |
  +--> Provider Adapter
  |
  +--> Tool Runtime
  |
  +--> Event Bus
         |
         +--> Session Store
         +--> Trace Store
         +--> Context Engine
         +--> UI Renderer
         +--> Optional exporters
```

External boundaries:

```text
Pi packages/extensions --> compatibility layer
MCP servers -----------> MCP client
Higher-level agents ----> MCP server / worker API
External KBs -----------> external-context refs
```

## 3. Recommended crate layout

Initial crate boundaries may be:

```text
crates/
  rupi-core/
  rupi-provider/
  rupi-session/
  rupi-context/
  rupi-trace/
  rupi-tools/
  rupi-mcp/
  rupi-pi-compat/
  rupi-extension/
  rupi-tui/

src/
  main.rs
```

Avoid premature crate proliferation.

Split only when a boundary has independent semantics or dependency pressure.

## 4. Agent runtime

The core agent loop owns:

- user turn intake;
- provider invocation;
- streamed output handling;
- tool dispatch;
- event emission;
- retry/failover coordination;
- handoff to context policy at safe boundaries.

The agent loop must not own:

- domain-specific workflows;
- MCP-specific business logic;
- Pi extension implementation details;
- rendering policy;
- storage formatting.

The CLI supplies a short native coding-agent system prompt on every session and appends
the discovered skill-control prompt when skills are available. A headless turn that ends
at its model-request budget is durably completed with `TurnStatus::BudgetExhausted` and a
flushed trace. `rupi run` exits successfully for that resumable partial outcome; the exit
status does not claim the requested task is complete. Provider and persistence failures
remain errors. Budget exhaustion ends the one-shot session as `Interrupted`; a completed
one-shot run and an explicit interactive exit remain `UserExit`.

## 5. Provider abstraction

Provider adapters normalize:

- model identity;
- capabilities;
- context window;
- max output;
- text/image support;
- tool-call support;
- reasoning exposure;
- provider-specific streaming;
- retry-relevant errors.

Conceptual capability model:

```rust
pub struct ModelCapabilities {
  pub text: bool,
  pub images: bool,
  pub tools: bool,
  pub exposed_reasoning: bool,
  pub context_window: u64,
  pub max_output_tokens: Option<u64>,
}
```

Provider errors should normalize into typed failure classes.

Conceptual categories:

```rust
pub enum ModelFailureKind {
  Transport,
  Timeout,
  RateLimited,
  ProviderUnavailable,
  Authentication,
  Protocol,
  ContextOverflow,
  Semantic,
  Cancelled,
}
```

Only explicit categories should qualify for automatic failover.

## 6. Event model

Important runtime behavior must emit typed events.

Conceptual event families:

```rust
pub enum AgentEvent {
  SessionStarted,
  UserMessage,
  ModelRequestStarted,
  ReasoningDelta,
  AssistantDelta,
  ModelRequestCompleted,
  ToolRequested,
  ToolStarted,
  ToolCompleted,
  ToolFailed,
  ExternalContextRetrieved,
  ContextReduced,
  ContextCompactionStarted,
  ContextCompactionCompleted,
  CheckpointCreated,
  ModelRetry,
  ModelFailover,
  SessionEnded,
}
```

Each event should carry stable identity and ordering metadata where meaningful:

```text
event_id
session_id
turn_id
timestamp
model_epoch
provider
model
tool_call_id
parent_event_id
trace_id
span_id
```

The event stream should be the shared source for:

- durable trace;
- UI updates;
- replay;
- session state reconstruction;
- failover continuity;
- diagnostics.

## 7. Session and trace separation

Maintain two logical representations.

### Session state

Semantic state needed for:

- active context;
- resumption;
- branching;
- Pi-compatible import/export.

### Trace

High-resolution historical execution.

A journal line carries an inline budget. Above it, whole fields move to the session's content-addressed blob store and the line keeps a bounded preview naming the reference and the original size, plus an `externalized` record beside it so a program can follow the pointer without parsing prose. Envelope bookkeeping and pointer-shaped fields (`*_id`, `*_ref`, `hash`, a `blob` record) are never elided: a line that cannot be attributed, or a pointer that cannot be followed, is worse than a long line. Redaction runs first, so the bytes that leave the line are already sanitized.

Blob payload compression is optional and disabled by default. When enabled, the store prefers raw Deflate only when it reduces the logical payload; the persisted `BlobRef` records the encoding suffix while its hash and size remain those of the redacted uncompressed bytes. Existing raw references remain readable, and the append-only JSONL journal itself is never compressed so tail recovery, inspection, and export remain plain-file operations.

Durable layout (the session files are kept flat so listing only reads headers):

```text
.rupi/
  sessions/
    <session-id>.jsonl              semantic resume projection
    <session-id>.trace.jsonl        canonical ordered trace
    <session-id>.wal.jsonl          crash-recovery projection intents
    <session-id>/
      blobs/
      checkpoints/
  leases/
    <session-id>.lease                 exclusive session ownership
  artifacts/
```

Exact paths remain configurable. A committed WAL is compacted; an incomplete
intent blocks read-only continuation until resume repairs it or fails closed.
Message-bearing runtime events use one WAL transaction with an exact redacted
message payload: small messages stay inline, while larger messages keep a verified
session-blob reference. `begin` and `resume` hold the per-session lease for the
handle lifetime, and retention acquires the same lease before deleting a victim.

## 8. Reasoning provenance

Reasoning-like information must use explicit provenance.

```rust
pub enum ReasoningProvenance {
  Native,
  ProviderSummary,
  Declared,
  Reconstructed,
}
```

Rules:

- `Native` means actually emitted by the model/provider.
- `ProviderSummary` is provider-generated transformed reasoning.
- `Declared` is intentionally requested explanation.
- `Reconstructed` is post-hoc inference.

Where the claim comes from:

- Provenance is decided by what the endpoint *declares* it exposes, never by which
  response field the text arrived in. The same `reasoning_content` field carries the
  model's own thinking on one server and a provider-authored summary on another, and
  a claim of `Native` for the latter reports hidden chain of thought as recovered.
- An endpoint that declares nothing leaves the field name as the only evidence, and
  that evidence is admitted only for the fields known to carry native thinking.
- The claim travels unchanged: provider event, journal record, rendered line.

Never serialize or render these as equivalent.

That rule is pinned at every hop a claim crosses, in `tests/provenance_roundtrip.rs`:

- the claim is a required field, on the provider event, on the trace line, and inside a
  session message's reasoning chunk. There is no `Default` for `ReasoningProvenance`, so
  a producer states one or does not compile;
- a trace line that has lost its claim is a malformed line. The journal counts it as
  damaged and refuses it; it is never read back as `Native`, which is the one weakening
  that would turn someone else's summary into reported model thought;
- each claim renders under its own label and its own style role, and a resumed session
  reports the same claim the run recorded, including the optional source detail;
- `Native` is the only claim that may be described as emitted reasoning, and only
  `Reconstructed` is described as rupi inference. Those two predicates are what the
  prose is generated from, so they are asserted directly.

## 9. Tool runtime

All tool execution should have durable lifecycle state.

```rust
pub enum ToolExecutionState {
  Requested,
  Started,
  Succeeded,
  Failed,
  Unknown,
}
```

Every tool call should have a stable ID.

An adapter that receives a model-authored call with malformed JSON arguments or ambiguous
fragment correlation emits `ProviderEvent::ToolCallRejected` with a stable call ID. The
runtime records the request and a terminal failed result, returns that result to the same
model for correction, and never dispatches the rejected call. Missing provider IDs are
replaced with internal IDs when a provider index still identifies the call. A fragment
without either key may attach only when exactly one explicitly keyed call without a prior
correlation conflict is open. Otherwise it remains rejected and receives
an internal ID only for failed-lifecycle reporting; ambiguous builders are not guessed. No
argument repair is executed as a tool request.

Once an assistant tool-call message is committed, every call receives exactly one
model-visible terminal tool result before another provider request is allowed. Cancellation
or finalization marks calls proven not to have started as `Failed`; uncertain side-effect
boundaries remain `Unknown`.

Mutating tool operations must not be blindly replayed after an uncertain failure boundary.

Read-only tools may use more permissive retry semantics.

Tool implementations should declare relevant metadata when possible:

```rust
pub struct ToolMetadata {
  pub name: String,
  pub read_only: bool,
  pub idempotent: bool,
}
```

### Opt-in progress boundary

Coding workflows may configure `RuntimeLimits::max_model_requests_without_progress` and
an optional `progress_tool_names` allowlist. After the configured number of tool-bearing
requests without one of those tools, `TurnLoop` records a runtime-owned model-visible
instruction and exposes only the allowlisted tools on the next request. With no allowlist,
all permitted mutating tools are exposed. The boundary is a bounded nudge, not a claim that
the host changed: the normal `Requested`/`Started`/`Succeeded`/`Failed`/`Unknown` lifecycle
still decides what actually happened. A successful configured progress tool satisfies the
one-shot boundary for the rest of that turn, and callers must verify the workspace
independently.
The default is disabled so read-only questions and inspection workflows remain unchanged.

## 10. Context engine

The context engine owns model-visible working memory.

It consumes canonical session/trace state and produces a bounded working set.

Before policy evaluation, request sizing includes the assembled system prompt, messages, and
currently exposed tool schemas. Provider-reported usage describes the request just sent; it
never substitutes for the estimate of the request being assembled. The active provider's
context window controls threshold evaluation after failover; adaptive latency observations
are scoped to model identity. Backup rebudgeting assembles that provider's system prompt,
retained messages, and exposed tools before committing the epoch transition. Same-turn
compaction validates every tool lifecycle in the proposed prefix and stops before any
unresolved or `Unknown` result.

Conceptual action enum:

```rust
pub enum ContextAction {
  Keep,
  Warn,
  ReducePayload,
  Compact,
  SuggestCheckpoint,
}
```

Policy should remain separable from mechanism.

Possible interface:

```rust
pub trait ContextPolicy {
  fn evaluate(&self, state: &ContextState) -> ContextAction;
}
```

### Context levels

- L0: payload reduction/eviction.
- L1: ordinary compaction.
- L2: semantic phase compaction.
- L3: checkpoint/reset.

Provider-reported context overflow is a reactive reliability path, not a policy
threshold. If the failed request committed no reasoning, assistant text, or decoded
tool call, the active turn may compact only the message prefix that predates the turn,
using one bounded local summary and the exact normal request shape, then reissue once.
All current-turn messages remain verbatim and in order. A second refusal, an overflow
after committed output, or a candidate that cannot fit is terminal. Context overflow
never activates model failover.

### Structured capsules

Prefer typed semantic state over free-form summaries.

Core fields should cover:

- objective;
- completed work;
- decisions;
- constraints;
- current state;
- important artifacts;
- unresolved items;
- next actions.

The capsule schema must be versioned.

### Checkpoint barrier invariant

A checkpoint capsule (L3) forms an impermeable barrier for ordinary compaction (L1/L2):
- Compaction operates only on messages accumulated after the most recent checkpoint.
- Compaction never crosses, mutates, or back-propagates across an established checkpoint capsule.
- When `ContextAction::SuggestCheckpoint` is evaluated under context pressure, the runtime synthesizes a structured capsule, archives it to durable storage (`checkpoints/<id>.json`), logs `CheckpointCreated`, advances the context compaction epoch, and resets model-visible messages to the structured capsule block.
- Session resume across a checkpoint initializes prompt context with the active capsule followed by post-checkpoint turns.

## 11. Context profiles

Built-in modes:

```text
aggressive
balanced
relaxed
```

`balanced` is the default.

Rules:

- lower thresholds automatically for constrained context windows;
- do not scale upward merely because a provider advertises a large window;
- keep numeric thresholds advanced and optional;
- compact only at safe runtime boundaries;
- preserve canonical trace state.

## 12. Model failover

One primary and one optional backup model.

Flow:

```text
request
  |
failure
  |
classify
  |
retry if eligible
  |
failover if eligible and retries exhausted
```

Before failover:

1. inspect backup capabilities;
2. ensure required modalities/tools exist;
3. compact/rebudget if context is too large;
4. record failover boundary;
5. continue from committed execution state.

When step 2 fails, failover is refused rather than attempted: the refusal names the
backup and the capability it lacks, and no request reaches that endpoint. An abstention
that is never stated is indistinguishable from a backup that was never configured, and
the operator cannot act on a decision they cannot see.

A context-window shortfall alone is not a refusal. It is a cost, and the takeover records
which gaps remained and whether history was actually shortened — not merely that the
backup's window was smaller.

After failover, the backup remains active until the user explicitly changes model.

Interactive session control:
- `/failover` triggers manual switch to backup model with `EpochReason::ManualSwitch`.
- `/switch-back` triggers manual return to primary model with `EpochReason::ManualSwitchBack`.
- Retries on qualifying availability failures apply exponential backoff (or server `retry-after`) and remain interruptible via `CancelToken`.

Do not auto-ping-pong.

## 13. Model epochs

A session may contain multiple model epochs.

Each epoch records:

- model;
- provider;
- start event;
- reason;
- capabilities snapshot.

Example reasons:

```text
initial
manual-switch
automatic-failover
```

Artifacts and reasoning should remain attributable to the epoch that produced them.

## 14. MCP client

MCP should normalize into the internal tool/resource model.

Rules:

- connection is lazy by default;
- discovery is lazy or filtered;
- do not inject all MCP schemas into every prompt;
- isolate protocol-version handling in the MCP layer;
- prefer semantic resource references over permanent prompt copying;
- stdio and the bounded Streamable HTTP adapter are supported; HTTP POST responses are
  bounded, JSON/SSE response ids are checked, session headers are carried, and protocol-owned
  headers cannot be overridden;
- long-lived server push remains deferred; one-shot HTTP calls run behind a per-request
  local relay authenticated by a per-attempt nonce, so cancellation closes and joins the
  in-flight worker without reposting; configured HTTP proxy routes are preserved;
- discovery passes through a bounded admission layer before registry/model exposure:
  provider-safe configured/tool name parts, descriptions up to 4 KiB, schemas up to 16 KiB,
  depth 32 and 4,096 nodes, 64 tools per server and 128 active tools overall, with per-server
  and aggregate metadata budgets. Empty schemas normalize to `{"type":"object"}`; invalid or
  over-budget catalogs fail closed instead of entering prompts.

## 15. MCP server / worker mode

`rupi-mcp::worker` exposes a coarse, explicit worker boundary through typed MCP
JSON-RPC. The embedding application supplies a headless [`WorkerEngine`] adapter (normally
composed from `TurnLoop`, `StoreTrace`, `CancelToken`, and `SilentProgress`); the MCP layer
owns run handles, cancellation, bounded waits, and stable resource projections.

The verified operations are:

```text
agent.start
agent.continue
agent.cancel
agent.branch
agent.compact
```

Resources use these stable URIs:

```text
session://<id>/state
session://<id>/summary
session://<id>/messages
session://<id>/trace
session://<id>/diff
session://<id>/artifacts
session://<id>/checkpoint/latest
```

`summary` is an external summary with declared/provider/runtime provenance; it is never
constructed by relabelling reasoning. `trace` is a bounded coarse projection containing
ordering and attribution, not raw event payloads or implementation paths. Diff and artifact
resources report explicit availability rather than fabricating data. The stdio dispatcher
accepts MCP `initialize`, `tools/list`, `tools/call`, `resources/list`, and `resources/read`
without scraping terminal output. Checkpoint and historical-event branching are now owned by
read-only `rupi-replay` plans; the worker still does not execute historical branches itself.

### 15.1 Replay and research tooling

`rupi-replay` is a pure projection boundary over redacted `TraceEntry` values joined with
optional `SessionRecord` messages. `rupi replay` is read-only: it never opens a provider,
executes a tool, or treats historical records as a new generation. Sequence numbers define
ordering; timestamps are used only for timing views. Filters, inclusive replay-until-event,
model-visible context snapshots, epoch/compaction/failover timelines, provenance summaries,
and trace export all derive from the same canonical records.

Historical branch plans carry a `HistoricalEventRef` and a context snapshot, and explicitly
separate copied history from a future generated continuation. A branch plan is not execution.
Unknown or mutating tool states remain marked for reconciliation and are never replayed blindly;
reasoning provenance remains attached and reconstructed rationale is never claimed as hidden
model reasoning.

### 15.2 Optimization experiments and adaptive policies

`rupi-experiments` defines pure evaluation and measurement boundaries for adaptive context
policies, standby backup analysis, and MCP capability exposure:
- **Context adaptation**: `KneeDetector` tracks `first_delta_ms` against context token estimates
  to detect non-linear prefill latency knees. `AdaptiveContextPolicy` only lowers or caps
  profile-derived thresholds when a knee occurs before `compact_tokens`; thresholds never exceed
  static profile limits, and adaptive mode remains opt-in (`RuntimeConfig.adaptive_context`).
- **Backup standby evaluation**: `evaluate_standby_tradeoff` models startup latency and RSS memory
  overheads against takeover speedup. Cold lazy backup remains the default execution posture.
- **MCP capability exposure**: `evaluate_mcp_exposure` measures token footprint across minimal,
  predictive prefetch, and eager strategies. Minimal exposure remains the default posture to
  protect model working context.
- **Timing provenance**: Request first-event/TTFT timing is recorded via optional, backward-compatible
  `ModelRequestCompleted.first_delta_ms`.

## 16. External context

Represent external knowledge through durable references.

The core contract is generic and serializable:

```rust
pub struct ExternalContextRef {
  pub provider: String,
  pub resource_id: String,
  pub citation: Option<String>,
  pub provenance: String,
  pub metadata: BTreeMap<String, String>,
}
```

`ExternalContextItem::compact_to_reference` changes only the model-visible working
representation; its provider identity and source metadata remain in the typed reference.
`ExternalContextItem::rehydrate` is a pure boundary helper. The provider-specific resolver
performs I/O and returns a fresh inline item, which the runtime records as another
`ExternalContextRetrieved` event. Retrieval messages are persisted with their typed
reference in `SessionMessage.external_context`, while canonical trace remains authoritative.

External context therefore supports:

- citation-aware prompt/UI rendering;
- compaction to a durable reference;
- later provider-owned rehydration;
- traceable source/provenance metadata;
- fail-closed unavailable-resource handling.

`rupi-rkb` is the first-party reference integration. It depends on generic core/MCP
contracts, while `rupi-core` has no dependency on RKB or its external crate.

## 17. Pi compatibility layer

Compatibility logic should remain isolated from the Rust-native runtime.

That boundary is a crate. `rupi-compat` reads files Pi already understands — skills and
prompt templates today — and returns typed state plus the list of things it did not
understand. Where two formats come from the same places, the *where* is stated once:
`scan` holds `Trust`, `Source`, and `Discovery`, because a trust decision that exists
twice is one that can disagree with itself. Expanding a template is pure text
transformation in `substitute`, with no file, process, or model in sight. It does
not import the runtime, the store, or a provider, and nothing it returns becomes runtime
state, an event, or a capability until the runtime deliberately adopts it. A field in
somebody else's file is evidence about that file, not a fact about this runtime.

Two rules govern the reading. Silence is reserved for what Pi itself skips without a
word; every other decision produces a warning naming the path and the reason, so an
empty result that means "we refused to look" cannot be mistaken for one that means
"there was nothing there". And a skill is instructions for the model — as is a prompt
template — so project-local locations are read only when the caller says the project is
trusted: `Trust` is
an input rather than a filesystem lookup because this runtime has no trust decision to
consult yet, and inventing one inside a file reader would be the worst place to hide it.

Priority order:

1. skills;
2. prompts;
3. package manifests/discovery;
4. package install (bounded explicit local copy; remote/dependency execution deferred);
5. session import/export;
6. extension tools/commands;
7. selected lifecycle events;
8. selected UI compatibility.

`rupi-extension` handles the selected TypeScript extension surface behind a typed
JSON-lines RPC boundary. It accepts trusted module paths, loads Pi-style default factories,
and returns normalized tool/command metadata, lifecycle/context results, and selected UI
notifications. Extension exceptions and process loss remain adapter errors; the core
session does not infer a successful tool result from a failed host call. Mutating extension
tools default to `Unknown` when completion is not observed.

The Node host must be lazy-started: constructing the host, inspecting compatibility
fixtures, and running a session without extension modules perform no process I/O. Project
extensions are never discovered or executed implicitly by the host.

## 18. TUI boundary

The TUI consumes semantic runtime events.

It should not own runtime state.

Visual rules:

- syntax-highlight operation vs argument vs prompt/path;
- visually distinguish reasoning provenance;
- make rare events prominent;
- collapse verbose output;
- avoid permanent dashboards;
- remain keyboard-first;
- keep render latency low.

## 19. Startup and lazy loading

Critical path:

```text
parse minimal config
identify project
restore lightweight metadata
initialize TUI
READY
```

Deferred by default:

- Node extension host (`rupi-extension`);
- MCP connections;
- backup provider connection/loading;
- deep trace hydration;
- external indexes;
- heavy package code;
- non-essential network calls.

Startup-path dependencies require stronger review.

## 20. Security

All durable trace output should pass through redaction policy before persistence.

Raw provider payload storage must be opt-in.

Project-local config must respect trust boundaries. Compatibility readers receive an
explicit caller-owned `Discovery::trust`; `rupi trust` persists exact canonical project
scopes in a private, schema-versioned file, but no reader infers trust from the files it
would activate.

Potentially dangerous behavior includes:

- subprocess spawning;
- extension loading;
- MCP activation;
- external checkpoint paths;
- native plugin activation.

## 21. Architecture invariants

1. Session state belongs to the runtime, not the active model.
2. Context is derived state.
3. Trace and working context are separate.
4. Reasoning provenance is explicit.
5. Tool side effects are never assumed across unknown completion state.
6. Automatic failover is availability-driven, not quality-driven.
7. Only one model is active in a normal execution role.
8. MCP is an adapter boundary, not the internal architecture.
9. Pi compatibility remains isolated and versioned.
10. Domain-specific logic remains outside core.
11. Optional systems do not block startup.
12. Performance-sensitive paths are measured, not guessed.
