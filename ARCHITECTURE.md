# ARCHITECTURE

## 1. Purpose

This document defines the implementation-facing architecture for `pi-rs`.

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
  pi-rs-core/
  pi-rs-provider/
  pi-rs-session/
  pi-rs-context/
  pi-rs-trace/
  pi-rs-tools/
  pi-rs-mcp/
  pi-rs-pi-compat/
  pi-rs-tui/

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

Possible layout:

```text
.pi-rs/
  sessions/
    <session-id>/
      session.jsonl
      trace.jsonl
      blobs/
      artifacts/
      checkpoints/
```

Exact paths remain configurable.

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
  `Reconstructed` is described as pi-rs inference. Those two predicates are what the
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

## 10. Context engine

The context engine owns model-visible working memory.

It consumes canonical session/trace state and produces a bounded working set.

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
- prefer semantic resource references over permanent prompt copying.

## 15. MCP server / worker mode

Later, `pi-rs` should expose coarse agent-level semantics.

Candidate operations:

```text
agent.start
agent.continue
agent.cancel
agent.branch
agent.compact
```

Candidate resources:

```text
session://<id>/state
session://<id>/summary
session://<id>/messages
session://<id>/trace
session://<id>/diff
session://<id>/artifacts
session://<id>/checkpoint/latest
```

Do not expose internal implementation details unless necessary.

## 16. External context

Represent external knowledge through durable references.

Conceptual type:

```rust
pub struct ExternalContextRef {
  pub provider: String,
  pub resource_id: String,
  pub citation: Option<String>,
  pub provenance: String,
}
```

External context should support:

- prompt rendering;
- compaction to reference;
- later rehydration;
- traceable provenance.

`rkb-rs` is the first planned reference integration.

## 17. Pi compatibility layer

Compatibility logic should remain isolated from the Rust-native runtime.

Priority order:

1. skills;
2. prompts;
3. package manifests/discovery;
4. package install;
5. session import/export;
6. extension tools/commands;
7. selected lifecycle events;
8. selected UI compatibility.

A Node host should handle TypeScript extension execution when needed.

The Node host must be lazy-started.

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

- Node extension host;
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

Project-local config must respect trust boundaries.

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
