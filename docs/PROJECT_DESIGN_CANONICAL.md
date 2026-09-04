---
title: "Project Design Canonical"
author: "Sae-Hwan Park"
date: 2026-09-04
---

**Status:** Canonical design draft  
**Project:** `pi-rs`  
**Primary language:** Rust  
**Design lineage:** Heavily inspired by Pi  
**Document purpose:** Define the stable project philosophy, architectural boundaries, runtime primitives, UX principles, compatibility goals, and staged implementation direction for `pi-rs`.

---

## 1. Project Definition

`pi-rs` is a clean Rust reimplementation of the core ideas behind Pi, designed as a minimal coding-agent runtime with stronger first-class support for:

- execution observability and provenance;
- local and remote models;
- long-running context lifecycle management;
- graceful model failover;
- MCP-based interoperability;
- Pi ecosystem compatibility;
- provenance-aware external knowledge;
- replayable, inspectable agent execution.

The project is **not** intended to be a source rewrite, fork, or Rust translation of Pi internals.

It should instead preserve Pi's most valuable behavioral and philosophical properties while making a small number of runtime capabilities foundational where doing so substantially improves reliability, transparency, composability, or local-model usability.

A concise project description is:

> **`pi-rs` is a minimal, Pi-inspired and Pi-compatible agent runtime for observable, context-efficient, fault-tolerant coding sessions across local and remote models.**

A shorter design mantra is:

> **Minimal core. Compatible ecosystem. Observable execution. Honest provenance. Recoverable state.**

---

## 2. Why `pi-rs` Exists

The project should not exist merely because Rust is appealing.

A language-only port would provide insufficient differentiation and could fragment the Pi ecosystem without creating enough value.

`pi-rs` is justified by a specific architectural thesis:

> **The coding-agent runtime should remain small, but execution state, provenance, context lifecycle, interoperability, and recovery should be explicit runtime primitives rather than loosely coupled afterthoughts.**

This matters particularly for:

- experimental open-weight models;
- local inference servers;
- long-running coding sessions;
- agent behavior auditing;
- higher-level orchestration;
- context-sensitive latency;
- model/provider instability;
- reproducible research into agent execution.

Rust is the substrate that makes a compact native implementation, strong typed boundaries, predictable resource management, and low-overhead execution attractive.

Rust is not the project's raison d'être.

---

## 3. Foundational Design Philosophy

### 3.1 Preserve Pi's core philosophy

`pi-rs` should benchmark against and learn heavily from Pi's design language.

The project should preserve, as far as practical:

- a small conceptual agent loop;
- model/provider flexibility;
- terminal-first interaction;
- plain and inspectable sessions;
- user-controlled workflows;
- extensibility over built-in feature accumulation;
- package/skill/prompt-based customization;
- a low-friction default experience.

`pi-rs` should feel familiar to Pi users.

Where it differs, the difference should usually come from stronger runtime semantics rather than from adding workflow complexity.

### 3.2 Reimplementation, not rewrite

Compatibility should be behavioral.

The project should not mechanically reproduce Pi's implementation architecture when Rust-native boundaries are cleaner.

The core question should be:

> Does this preserve the expected semantics and ecosystem behavior?

not:

> Does this look internally like Pi?

### 3.3 Minimalism means minimal fundamental primitives

Minimalism should not be interpreted as "the core does almost nothing."

A runtime primitive belongs in core when it is fundamental to reliable execution and when every implementation of the feature would otherwise need to reproduce the same fragile machinery.

This allows core support for:

- event tracing;
- context lifecycle;
- session persistence;
- model failover;
- provider capability checks;
- tool transaction state;
- interoperability boundaries.

It should still exclude:

- multi-agent workflow policy;
- domain-specific research pipelines;
- opinionated software-development methodology;
- model voting;
- project-management systems;
- built-in browser or domain logic when extensions or MCP are appropriate.

### 3.4 Context is a cache, not the record

This is one of the project's central invariants.

The model's active prompt is a replaceable working set.

The durable execution trace is the canonical record.

```text
append-only execution history
            |
            +-------------------+
            |                   |
            v                   v
         replay            context engine
                                |
                                v
                       bounded working set
                                |
                                v
                              model
```

Compaction may remove information from active model context.

It must not silently erase the canonical execution record.

### 3.5 Working memory is disposable; evidence should be recoverable

Information should not be modeled as only:

```text
remembered
forgotten
```

A better model is:

```text
resident
compacted
rehydratable
```

This is especially important for:

- large tool results;
- external documentation;
- RKB evidence;
- prior coding episodes;
- archived checkpoints;
- reproducible traces.

### 3.6 Honest provenance

Any reasoning-like content must identify where it came from.

The runtime must never imply access to hidden chain-of-thought that a provider did not expose.

The user should always be able to distinguish:

- actual model-emitted reasoning;
- provider-generated reasoning summaries;
- intentionally declared rationales;
- post-hoc reconstructed rationales.

### 3.7 One model is active at a time

Normal `pi-rs` execution is not an ensemble.

The core runtime may have:

- one primary model;
- one optional backup model.

Only one model drives the agent at a time.

Multi-model orchestration, voting, judging, and delegation belong outside the core or in explicit extensions.

### 3.8 Conservative automation

Automatic runtime behavior should be predictable.

Examples:

- retries before failover;
- compaction only at safe boundaries;
- no silent destructive reset;
- no blind replay of uncertain side effects;
- no arbitrary quality-based model switching;
- no eager connection of every configured MCP server;
- no automatic injection of large external evidence when references suffice.

### 3.9 Quiet by default; rich when inspected

The interface should not expose every internal mechanism constantly.

Normal interaction should remain calm.

Detailed provenance, trace data, context statistics, MCP information, and reasoning history should be available when requested.

### 3.10 Instant before complete

The runtime should become usable before every optional subsystem finishes initialization.

The engineering version is:

> **Eager only for the critical path; lazy everywhere else.**

---

## 4. Project Goals

### 4.1 Primary goals

`pi-rs` should:

1. remain recognizably Pi-like in interaction and philosophy;
2. support common Pi ecosystem artifacts with little or no modification;
3. run as a small, responsive native Rust application;
4. support both local and remote models;
5. preserve exposed reasoning and execution provenance;
6. provide explicit reasoning provenance;
7. support long-running sessions through native context lifecycle management;
8. preserve canonical trace state across compactions;
9. expose and consume MCP capabilities;
10. support external orchestration without embedding orchestration policy;
11. gracefully continue through primary model/provider failures when a backup is configured;
12. make external evidence durable and rehydratable;
13. require little configuration for normal use.

### 4.2 Secondary goals

The project should also be suitable for:

- local-model experimentation;
- agent behavior research;
- execution auditing;
- reproducible debugging;
- replay and post-hoc analysis;
- integration into larger agent systems;
- development of domain-specific first-party extensions.

---

## 5. Explicit Non-Goals

The initial core should not become:

- a multi-agent framework;
- an autonomous software-development organization simulator;
- a model router for quality/cost optimization;
- an ensemble/voting system;
- a judge-model framework;
- a built-in browser automation suite;
- a domain-specific knowledge base;
- a project-management platform;
- a hidden-chain-of-thought extraction system;
- a dashboard-heavy terminal application.

These capabilities may exist outside core through:

- extensions;
- skills;
- MCP;
- external orchestrators;
- companion applications.

---

## 6. High-Level Architecture

```text
+---------------------------------------------------------+
|                         pi-rs                           |
|                                                         |
|   +-------------+        +-------------------------+    |
|   | Agent Loop  |<------>| Provider Abstraction    |    |
|   +------+------+        +-----------+-------------+    |
|          |                           |                  |
|          +-------------+-------------+                  |
|                        v                                |
|                 +-------------+                         |
|                 | Event Bus   |                         |
|                 +------+------+                         |
|                        |                                |
|       +----------------+-------------------+            |
|       |                |                   |            |
|       v                v                   v            |
| Session Store    Context Engine       Trace/Replay      |
|       |                |                                |
|       |         +------+------+                         |
|       |         |             |                         |
|       |      Artifacts    External Context              |
|       |                       |                         |
|       +-----------------------+-------------------------+
|                               |
|                         MCP / extensions
+-------------------------------+-------------------------+
                                |
                  +-------------+--------------+
                  |             |              |
                rkb-rs        GitHub         Browser
```

The reverse direction supports orchestration:

```text
higher-level orchestrator
          |
         MCP
          |
          v
        pi-rs
          |
     coding session
```

---

## 7. Core Runtime Components

A possible crate-level decomposition is:

```text
pi-rs-core
pi-rs-provider
pi-rs-session
pi-rs-context
pi-rs-trace
pi-rs-tools
pi-rs-mcp
pi-rs-pi-compat
pi-rs-cli
pi-rs-tui
```

Exact crate boundaries may evolve.

The conceptual boundaries should remain stable:

- agent execution;
- provider normalization;
- session state;
- context lifecycle;
- tools;
- event tracing;
- interoperability;
- Pi compatibility;
- terminal UX.

---

## 8. Event-Driven Runtime

Important runtime actions should produce typed events.

A conceptual schema:

```rust
enum AgentEvent {
  SessionStarted,
  UserMessage,
  ModelRequest,
  ReasoningDelta,
  AssistantDelta,
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

Events should carry relevant metadata such as:

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

The same event stream should support:

- session persistence;
- TUI rendering;
- trace output;
- replay;
- model failover reconstruction;
- context lifecycle;
- OpenTelemetry or other optional exporters.

Avoid separate ad hoc logging systems for each subsystem.

---

## 9. Session State and Trace Storage

Two logical representations should exist.

### 9.1 Semantic session state

Suitable for:

- resumption;
- branching;
- user inspection;
- Pi compatibility;
- reconstruction of active context.

Example:

```text
session.jsonl
```

### 9.2 High-resolution trace

A detailed event journal:

```text
trace.jsonl
```

Large payloads should be stored separately when appropriate:

```text
artifacts/
blobs/
tool-results/
```

The runtime should be able to reconstruct what happened without loading every large payload into active memory.

---

## 10. Reasoning Transparency and Provenance

Reasoning-like information should use explicit provenance categories.

### 10.1 Native reasoning

Actual reasoning content emitted by the model/provider.

```text
provenance = native
```

Preserve it as faithfully as practical.

### 10.2 Provider summary

A provider-generated summary or transformed representation of hidden reasoning.

```text
provenance = provider_summary
```

Never describe this as raw chain-of-thought.

### 10.3 Declared rationale

An optional compact explanation intentionally requested from the model around meaningful decisions.

Example:

```json
{
  "intent": "change the cache API",
  "evidence": [
    "all callers are internal"
  ],
  "expected_effect": "remove duplicated validation"
}
```

```text
provenance = declared
```

This is a model-generated explanation, not recovered private reasoning.

### 10.4 Reconstructed rationale

A post-hoc inference based on:

- visible context;
- tool calls;
- tool results;
- edits;
- later behavior.

```text
provenance = reconstructed
```

It should render explicitly as inferred:

```text
≈ RECONSTRUCTED RATIONALE
Not original model reasoning.
```

---

## 11. Replay

Replay should become a first-class capability after the foundational runtime is stable.

Potential interfaces:

```bash
pi-rs replay session.jsonl
pi-rs replay session.jsonl --tools
pi-rs replay session.jsonl --reasoning
pi-rs replay session.jsonl --timing
pi-rs replay session.jsonl --until event:381
```

Longer-term possibilities:

- branch from an historical event;
- reconstruct exact model-visible context at a given point;
- compare model continuations from the same historical state;
- inspect compaction effects;
- visualize failover epochs;
- audit tool side effects.

Historical execution must remain clearly separated from newly generated continuation.

---

## 12. Pi Ecosystem Compatibility

Compatibility should be explicit, versioned, and tested.

A likely target hierarchy:

| Surface | Target |
|---|---|
| Skills / `SKILL.md` | Very high |
| Prompt templates | Very high |
| Package discovery | Very high |
| Package installation | Very high |
| Package manifests | High |
| Session import/export | High |
| Themes | High |
| Extension tools | High |
| Extension commands | High |
| Extension lifecycle events | High |
| Complex custom UI | Best effort |
| Arbitrary Pi internal imports | Not guaranteed |

A compatibility tool could eventually report:

```text
Pi compatibility

✓ skills
✓ prompts
✓ registerTool
✓ registerCommand
△ custom TUI widget
✗ internal Pi module import
```

Compatibility should be measured through fixtures and integration tests rather than broad claims.

---

## 13. TypeScript Extension Compatibility

Existing Pi TypeScript extensions are strategically important.

Do not force them to become Rust extensions.

A Node compatibility host is preferred initially:

```text
                 pi-rs
                   |
             extension RPC
                   |
                   v
        +----------------------+
        | Node extension host  |
        +----------------------+
                   |
              Pi extensions
```

Advantages:

- npm dependencies remain available;
- arbitrary Node packages continue to work;
- existing Pi extension code may run with minimal changes;
- the Rust process remains isolated from JavaScript complexity.

Later, a native extension path may coexist:

```text
TypeScript extensions -> compatibility path
Rust/WASM extensions  -> native path
```

The Node host should start lazily only when required.

---

## 14. Context Lifecycle System

Context lifecycle management belongs in the runtime.

It should generalize the design lessons from `local-context-manager`.

The system should maintain:

```text
1. FORENSIC TRACE
   complete observable history

2. WORKING CONTEXT
   bounded prompt supplied to the model

3. DURABLE SEMANTIC STATE
   checkpoints, capsules, artifact references
```

The working context may forget.

The trace must not.

---

## 15. Context Management Levels

### L0: Payload reduction / eviction

Oversized tool results or external evidence need not remain resident in the prompt.

Example:

```text
Full result:
  18,000 tokens
```

may become:

```text
Large result archived as artifact://abc123

Relevant excerpts:
...

Retrieve artifact if full detail becomes necessary.
```

The original remains durable.

### L1: Ordinary compaction

Older conversational content is summarized while recent context remains verbatim.

### L2: Semantic phase compaction

Compaction can occur because a meaningful task phase has ended, not merely because the context window is nearly full.

Good boundaries include:

- implementation complete;
- tests passing;
- debugging episode resolved;
- investigation concluded;
- PR ready.

### L3: Episode checkpoint/reset

A larger semantic episode may be archived and replaced by a continuation capsule.

Useful boundaries include:

- PR merged;
- issue resolved;
- release completed;
- deployment completed;
- research episode finished.

Session-changing actions should remain reviewable.

---

## 16. Structured Context Capsules

Compaction should prefer structured semantic state over a single free-form summary.

Example:

```yaml
objective:
  Fix lifetime handling in session cache.

completed:
  - Located stale-entry bug.
  - Changed cache invalidation logic.
  - Added regression tests.

decisions:
  - Preserve current public Cache API.
  - Avoid Arc<RwLock<_>> migration.

current_state:
  tests: passing
  working_tree: clean

important_artifacts:
  - src/cache.rs
  - tests/cache_regression.rs

constraints:
  - preserve API compatibility

unresolved:
  - benchmark allocation impact

next_actions:
  - run benchmarks
  - open PR
```

The semantic representation may later be rendered differently for different models.

---

## 17. Context Profiles

Most users should not tune thresholds manually.

Built-in profiles:

```text
aggressive
balanced
relaxed
```

`balanced` is the invisible default.

Selection should be symptom-driven:

```text
Growing sessions become sluggish -> aggressive
Everything feels comfortable     -> balanced
Compaction happens too often     -> relaxed
```

The model's reported context window may lower thresholds for constrained models.

Large advertised windows should not automatically increase thresholds.

Advanced numerical configuration can exist for specialized benchmarking.

---

## 18. Future Adaptive Context Policy

A later experimental subsystem may learn context-performance behavior from:

```text
model context size
+
observed prefill latency
+
KV-cache behavior
+
cache hits
+
hardware/runtime
         |
         v
dynamic effective context policy
```

For example, a local model may technically support 128k tokens while becoming unpleasantly slow above 30k.

The runtime could eventually learn this performance knee.

This should not be required for the initial implementation.

---

## 19. MCP Client

MCP should be a first-party interoperability subsystem.

MCP capabilities should normalize into the same internal tool abstraction used by native tools and extensions.

Conceptually:

```rust
Tool
  |- BuiltinTool
  |- ExtensionTool
  `- McpTool
```

The model should not need to care whether a capability is implemented natively, through an extension, or by MCP.

---

## 20. Lazy MCP Capability Exposure

A configured environment may contain hundreds of MCP tools.

Injecting every schema into model context is undesirable, especially for local models.

`pi-rs` should support lazy discovery or filtering.

Potential mechanisms:

```text
/mcp enable github
/mcp enable rkb
```

or:

```text
mcp.search_capabilities(...)
mcp.call(...)
```

or skill-driven activation.

The exact design can remain experimental.

The invariant is:

> Connecting MCP servers must not imply permanent exposure of every tool schema to every model turn.

---

## 21. MCP Server / Worker Mode

`pi-rs` should also be able to expose itself as an MCP-accessible worker.

Potential semantic operations:

```text
agent.start
agent.continue
agent.cancel
agent.branch
agent.compact
```

Potential resources:

```text
session://<id>/state
session://<id>/summary
session://<id>/messages
session://<id>/trace
session://<id>/diff
session://<id>/artifacts
session://<id>/checkpoint/latest
```

The external interface should expose coarse, stable agent semantics rather than every internal event.

---

## 22. Higher-Level Orchestration Boundary

Higher-level orchestration belongs outside the runtime.

Example:

```text
                  ORCHESTRATOR
                       |
            +----------+----------+
            |          |          |
            v          v          v
         pi-rs A    pi-rs B    pi-rs C
         backend    frontend    reviewer
```

Each `pi-rs` worker may independently be:

- stateful;
- inspectable;
- resumable;
- compactable;
- auditable;
- fault tolerant.

The orchestrator should operate mainly on semantic state rather than ingesting every worker trace.

This keeps the coding harness minimal while making it composable.

---

## 23. External Context and Rehydration

External knowledge should be represented as a durable reference rather than only pasted text.

Conceptually:

```rust
struct ExternalContextRef {
  provider: String,
  resource_id: String,
  citation: Option<String>,
  provenance: Provenance,
}
```

Working context may contain a temporary textual rendering.

The durable session should retain the reference.

This allows evidence to be:

```text
retrieved
  |
resident in context
  |
compacted to reference + conclusion
  |
rehydrated when needed
```

---

## 24. `rkb-rs` First-Party Integration

`rkb-rs` should remain an independent domain-specific project.

It should not become a core dependency.

Instead:

```text
pi-rs core
  `- generic external-context primitives

pi-rs-rkb
  `- official integration

rkb-rs
  `- independent knowledge system
```

The integration can provide:

- MCP setup/discovery;
- skills describing when to use RKB;
- citation-aware rendering;
- durable resource IDs;
- evidence-aware compaction;
- rehydration hooks.

`rkb-rs` should serve as an architectural reference implementation for provenance-aware external knowledge.

The generic feature extracted into `pi-rs` is not "RAG."

It is:

> **provenance-aware, rehydratable external context.**

---

## 25. Model Failover

Model failover is a core reliability feature, not orchestration.

A simple configuration:

```toml
[model]
primary = "local/qwen"
backup = "openai/gpt"
```

Only one model is active at a time.

---

## 26. Failover Semantics

The sequence should be deterministic:

```text
primary request
      |
    failure
      |
 classify
      |
 retryable?
  |       |
 yes      no
  |       |
bounded   |
retry     |
  |       |
fails ----+
      |
      v
backup model
```

Retry should precede failover.

### Eligible automatic failover cases

Examples:

- provider outage;
- local server unavailable;
- connection refusal;
- repeated transport failures;
- timeout after retries;
- provider 5xx;
- persistent rate limit;
- unrecoverable stream interruption;
- endpoint disappearance;
- repeated malformed provider/model protocol output.

### Non-failover cases

Do not automatically fail over because:

- the answer is mediocre;
- tests fail;
- the implementation is poor;
- the model appears confused;
- the model disagrees with the user.

Those are quality/orchestration concerns.

---

## 27. Mid-Turn Failover

The backup must continue from actual committed execution state.

Example:

```text
primary:
  read file
  edit file
  run test
  [connection lost]
```

The backup should see the committed read/edit/test events and continue from there.

The whole turn must not simply replay.

This requires the session to exist independently from the active model.

---

## 28. Tool Transaction State

Tool calls should have durable lifecycle states:

```text
Requested
Started
Succeeded
Failed
Unknown
```

A stable tool-call ID should be recorded.

If a side-effecting operation has `Unknown` completion state, the runtime must not blindly replay it after failover.

Read-only operations are more safely retryable.

This is a distributed-systems-style reliability invariant.

---

## 29. Backup Compatibility Gate

The backup may differ from the primary.

The runtime should inspect capabilities such as:

```text
                     primary   backup
text                    yes      yes
images                  yes      no
tool calling            yes      yes
context window          128k     32k
reasoning exposed       yes      no
```

If the current context exceeds backup capacity:

```text
primary fails
    |
backup capability check
    |
context too large
    |
emergency compaction
    |
backup continues
```

If a required modality or capability is unavailable, the failover should be rejected or explicitly degraded rather than failing mysteriously.

---

## 30. Model Epochs and Provenance

A session may contain multiple execution epochs.

```text
Session
  |- Epoch 1
  |   model = Qwen
  |   reason = initial
  |
  `- Epoch 2
      model = GPT
      reason = failover
```

Trace rendering should show failover boundaries clearly:

```text
MODEL qwen
REASONING [native]
TOOL read
TOOL edit
ERROR connection reset

------ MODEL FAILOVER ------
from: local/qwen
to: openai/gpt
reason: endpoint unavailable
----------------------------

MODEL gpt
TOOL test
...
```

Different reasoning provenance after failover must remain explicit.

---

## 31. Failover Recovery Policy

Avoid automatic model ping-pong.

Initial policy:

> After automatic failover, the backup remains active until the user explicitly switches back.

Possible commands:

```text
/failover
/model-primary
/model-backup
```

Exact naming may evolve.

Automatic primary recovery can be considered later at safe boundaries.

---

## 32. Local Models as First-Class Citizens

The runtime should treat local inference as a serious primary use case.

Relevant concerns include:

- long prefill latency;
- finite context windows;
- exposed reasoning streams;
- model-specific tool formats;
- local endpoint crashes;
- quantization-dependent performance;
- runtime-specific quirks;
- slower cold model loading.

No single local runtime should be mandatory.

Likely provider targets include:

- llama.cpp-compatible servers;
- OpenAI-compatible local endpoints;
- pluggable custom providers.

---

## 33. UX and Visual Design Philosophy

The visual design should follow Pi's interaction language while refining readability and information hierarchy.

The north star:

> **Minimal chrome, high information density, strong visual hierarchy, and almost no decorative UI.**

### 33.1 Semantic rather than decorative styling

Color and typography should encode meaning.

Examples:

- operation names;
- arguments;
- paths;
- free-form prompts;
- native reasoning;
- reconstructed rationale;
- warnings;
- errors;
- failover;
- context compaction.

Do not add visual effects merely to look modern.

### 33.2 Syntax-highlight commands and operations

Example:

```text
/read src/context.rs
^^^^^ operation
      ^^^^^^^^^^^^^^ argument
```

and:

```text
/compact-phase implementation complete; tests pass
^^^^^^^^^^^^^^ operation
               ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ prompt/reason
```

Tool execution should also be easy to skim:

```text
read        src/context.rs
edit        src/session.rs
test        cargo test -p pi-rs-context
```

The operation, structured argument, path, and free text should have consistent visual roles.

### 33.3 Provenance should have a visual grammar

The following should never look identical:

```text
NATIVE REASONING
PROVIDER SUMMARY
DECLARED RATIONALE
RECONSTRUCTED RATIONALE
```

They should differ through restrained labels, icons, or text treatment.

### 33.4 Rare events should stand out

Examples:

- model failover;
- destructive checkpoint/reset;
- unrecoverable tool uncertainty;
- emergency compaction;
- provider authentication failure.

Routine reads and successful tools should remain visually quiet.

### 33.5 Collapse detail by default

Long content should be inspectable without dominating the session:

- reasoning;
- tool output;
- MCP payloads;
- trace metadata;
- large diffs;
- archived artifacts.

### 33.6 Avoid dashboardification

Do not permanently occupy the screen with:

- token gauges;
- MCP status panels;
- provider dashboards;
- trace IDs;
- multiple persistent panes;
- activity widgets.

A compact status line is preferable:

```text
qwen-local · ctx 21k/131k · balanced · git:main
```

Important changes should surface contextually:

```text
context compacted 31k -> 12k
```

```text
primary unavailable -> backup: gpt
```

### 33.7 Keyboard-first, terminal-native

The TUI should prioritize:

- immediate typing;
- predictable keyboard shortcuts;
- fuzzy completion;
- narrow-terminal behavior;
- restrained spacing;
- fast scrolling;
- low render overhead.

Core usability should not depend on exotic terminal features.

---

## 34. Command Design

Keep top-level commands small and semantic.

Prefer:

```text
/model
/context
/trace
/mcp
/checkpoints
/failover
```

Use subcommands, fuzzy completion, and contextual help for complexity.

Avoid accumulating dozens of narrowly scoped top-level commands.

The user should be able to discover advanced functionality without memorizing it.

---

## 35. Performance and Responsiveness

Performance is a user-facing design requirement.

A terminal agent that starts slowly or blocks on optional initialization feels heavy regardless of implementation language.

### 35.1 Critical principle

> **Do the minimum work required for the next interaction.**

### 35.2 Startup path

Startup should ideally perform only:

```text
process start
   |
   |- parse minimal config
   |- identify project
   |- restore lightweight session metadata
   |- initialize TUI
   `- READY
        |
        |- load extensions lazily
        |- discover MCP lazily
        |- initialize backup lazily
        |- hydrate deep history lazily
        `- open heavy indexes only when needed
```

The editor should become usable before optional services are fully ready.

### 35.3 Readiness stages

Conceptually track:

```text
T0 executable starts
T1 UI appears
T2 user can type
T3 first prompt can be submitted
T4 optional services fully ready
```

Optimize primarily for `T1` through `T3`.

`T4` may happen progressively.

### 35.4 Aspirational latency budgets

Early engineering targets may include:

```text
warm startup to interactive       <100 ms
cold startup to interactive       <250 ms
keypress/render latency             <16 ms
slash completion                     <50 ms
local session metadata lookup       <50 ms
```

These are targets, not promises.

They should be measured and revised empirically.

### 35.5 Avoid startup tax accumulation

A recurring audit should prevent:

```text
config parsing       +40 ms
package scanning     +80 ms
MCP discovery       +150 ms
git inspection      +100 ms
session hydration   +200 ms
```

from silently turning into sluggish startup.

### 35.6 Startup-path dependencies deserve stronger scrutiny

Dependencies on the startup critical path should require clear justification.

Avoid unnecessary:

- directory scans;
- full-history parsing;
- database initialization;
- network calls;
- provider discovery;
- Node startup;
- MCP server startup.

---

## 36. Lazy Loading Strategy

### 36.1 Backup model

The configured backup should normally incur nearly zero runtime cost until needed.

Do not connect to or load a second model simply because it is configured.

Warm standby may exist later as an explicit opt-in.

### 36.2 MCP

Configured MCP servers should not all connect at startup.

Start or connect only when:

- their capability is requested;
- the relevant skill activates;
- the user explicitly enables them.

Cache connections and schemas afterward where appropriate.

### 36.3 Pi extensions

Package manifests may be discovered cheaply.

Extension implementations should load only when needed.

The Node compatibility host should not start unless a TypeScript extension requires it.

### 36.4 Session restoration

Do not parse an enormous historical trace just to resume a session.

Prefer:

```text
session index
   |
latest checkpoint
   +
post-checkpoint active events
   |
working state
```

Historical trace remains cold until inspected.

### 36.5 External indexes

Heavy SQLite, FTS, vector, or knowledge-base indexes should open on first use unless needed for the initial prompt.

---

## 37. Context Management as a Performance Feature

Context lifecycle is not only about avoiding overflow.

For local models:

```text
larger context
    |
slower prefill
    |
longer time before response
    |
worse interactive UX
```

Therefore:

> **Optimize context for useful information per unit of inference cost, not maximum context utilization.**

A model supporting 128k tokens does not imply that routinely using 100k+ is desirable.

Proactive compaction can function as an inference-latency controller.

---

## 38. Immediate Feedback

When work itself is slow, the interface should still acknowledge progress immediately.

Example:

```text
read  src/parser.rs
grep  parse_expr
edit  src/parser.rs
test  cargo test parser
```

For provider delays, a subtle status is enough:

```text
qwen-local · generating
```

Avoid excessive spinners, animations, or noisy progress indicators.

---

## 39. Security and Privacy

Observability creates risk because traces may contain:

- source code;
- `.env` contents;
- secrets returned by tools;
- credentials;
- database URLs;
- private documents;
- personal information;
- model reasoning that repeats sensitive values;
- provider metadata.

A redaction pipeline should exist before durable logging:

```text
raw event
    |
redaction policy
    |
semantic event
    +--> trace
    +--> UI
    `--> telemetry
```

Raw provider payload capture should be optional and disabled by default.

Example:

```toml
[trace]
raw_provider_payloads = false
```

Sensitive artifacts should use restrictive filesystem permissions.

---

## 40. Project Trust

Project-local configuration should not silently gain powerful behavior in an untrusted repository.

Trust-sensitive capabilities include:

- enabling MCP servers;
- spawning subprocesses;
- loading extensions;
- modifying context policies;
- writing checkpoints outside normal locations;
- enabling custom native extensions.

Global and project configuration should have explicit precedence and trust rules.

---

## 41. Configuration Philosophy

A normal user's configuration should be small.

Example:

```toml
[model]
primary = "local/qwen"
backup = "openai/gpt"

[context]
mode = "balanced"
```

Everything else should have sensible defaults.

Advanced options may include:

- retry policy;
- trace retention;
- redaction;
- context thresholds;
- MCP activation;
- extension hosts;
- checkpoint storage;
- startup/performance diagnostics.

Users should not need to understand them to use the product well.

---

## 42. Core vs First-Party vs External

| Capability | Location |
|---|---|
| Agent loop | Core |
| Provider abstraction | Core |
| Tool execution | Core |
| Session persistence | Core |
| Event store | Core |
| Reasoning provenance | Core |
| Context lifecycle | Core |
| Model failover | Core |
| Tool transaction semantics | Core |
| Pi compatibility | Core / compatibility subsystem |
| MCP protocol support | First-party core subsystem |
| Node extension host | First-party |
| `rkb-rs` integration | First-party extension |
| Browser integration | Extension / MCP |
| Multi-agent orchestration | External / extension |
| Model voting/judging | External / extension |
| Domain-specific workflows | Extensions / skills |
| Domain-specific knowledge | External service / extension |

A feature should enter core only when it is fundamentally about runtime reliability, inspectability, compatibility, context, or composability.

---

## 43. Testing Strategy

### 43.1 Compatibility fixtures

Maintain fixtures for:

- Pi skills;
- prompts;
- package manifests;
- sessions;
- extension APIs.

CI should detect compatibility regressions.

### 43.2 Deterministic provider simulation

A fake provider should simulate:

- successful streaming;
- exposed reasoning;
- malformed reasoning;
- interruption;
- timeout;
- 429;
- 500;
- context overflow;
- malformed tool calls;
- provider disappearance.

### 43.3 Failover tests

Cover:

```text
failure before output
failure after reasoning
failure after read-only tool
failure after write
uncertain side-effect completion
smaller backup context
backup lacking modality
backup failure
```

### 43.4 Context invariants

Compaction tests should verify retention of:

- active objective;
- explicit user constraints;
- unresolved work;
- important artifacts;
- pending actions.

The original trace must survive all working-context compactions.

### 43.5 Replay invariants

Replay should preserve:

- event order;
- model epochs;
- tool lifecycle state;
- context transformations;
- provenance labels.

### 43.6 Performance tests

Measure at minimum:

- cold startup;
- warm startup;
- first keystroke readiness;
- session resume;
- slash completion;
- rendering latency;
- package discovery;
- Node host activation;
- MCP first-use latency.

Performance regressions should be visible in CI where practical.

---

## 44. Development Roadmap

### Phase 0: Design and contracts

Deliver:

- architecture document;
- canonical principles;
- compatibility matrix;
- event schema;
- provider abstraction;
- session format;
- explicit non-goals;
- UX/performance budgets.

### Phase 1: Minimal usable agent

Implement:

- CLI;
- minimal TUI;
- provider interface;
- one local provider path;
- one cloud-compatible path;
- basic coding tools;
- session persistence;
- responsive startup.

Goal:

> A small, useful coding agent exists before the advanced architecture grows around it.

### Phase 2: Event trace and provenance

Implement:

- event bus;
- high-resolution traces;
- reasoning provenance;
- tool lifecycle events;
- model epochs;
- basic trace inspection.

This should happen early because most later features depend on trustworthy events.

### Phase 3: Pi compatibility foundation

Implement:

- skills;
- prompts;
- package discovery;
- package installation;
- compatibility fixtures;
- session import/export where practical.

### Phase 4: Context lifecycle

Generalize the proven ideas from `local-context-manager`:

- context statistics;
- profiles;
- proactive safe-boundary compaction;
- oversized tool-result reduction;
- semantic phase compaction;
- checkpoint/reset;
- structured capsules;
- fast session restoration.

### Phase 5: Model failover

Implement:

- failure classification;
- bounded retry;
- primary/backup configuration;
- model epochs;
- backup capability checks;
- context rebudgeting;
- side-effect safeguards.

### Phase 6: MCP client

Implement:

- protocol abstraction;
- lazy server connection;
- tool discovery;
- capability filtering;
- tool normalization;
- protocol negotiation.

Use `rkb-rs` as an early integration fixture.

### Phase 7: `rkb-rs` first-party integration

Implement:

- setup/discovery;
- skill integration;
- provenance-preserving results;
- durable external references;
- evidence-aware compaction;
- rehydration.

Use this phase to refine generic external-context primitives.

### Phase 8: TypeScript extension compatibility

Implement:

- Node extension host;
- tool registration;
- commands;
- lifecycle events;
- context hooks;
- selected UI facilities.

Expand based on real ecosystem needs.

### Phase 9: MCP server / worker mode

Implement:

- start/resume/cancel;
- session resources;
- artifacts;
- summaries;
- trace access;
- long-running task semantics.

### Phase 10: Replay and research tooling

Add:

- visual replay;
- historical context reconstruction;
- branch-from-event;
- model comparison;
- trace analysis;
- reasoning/provenance inspection.

### Phase 11: Adaptive optimization experiments

Only after stable baselines:

- learned context-performance knees;
- optional warm backup;
- smarter MCP activation;
- adaptive prefetching;
- deeper latency optimization.

---

## 45. Initial MVP Boundary

The first public MVP should remain intentionally smaller than the full vision.

A strong MVP may include:

- native Rust binary;
- minimal sleek TUI;
- one local and one remote provider path;
- basic coding tools;
- persistent sessions;
- event trace;
- native reasoning preservation;
- basic Pi skills/prompts;
- profile-based context management;
- primary + backup model;
- MCP client;
- low-latency startup.

Do not delay the MVP for:

- perfect Pi extension compatibility;
- sophisticated replay UI;
- full MCP worker mode;
- autonomous context adaptation;
- multi-agent features.

---

## 46. Success Criteria

### 46.1 Compatibility

A Pi user can move common skills and packages to `pi-rs` with little friction.

### 46.2 Minimalism

A new user can run the harness without learning a framework.

### 46.3 Responsiveness

The tool feels immediate at startup and during ordinary interaction.

Optional integrations do not block the editor.

### 46.4 Local-model robustness

Long-running sessions remain usable despite:

- slow prefill;
- finite context;
- local server instability;
- experimental model behavior.

### 46.5 Auditability

A user can answer:

- Which model generated this?
- What reasoning was actually exposed?
- What was reconstructed?
- Which tools had run?
- Which side effects succeeded?
- What evidence had the model seen?
- When did compaction occur?
- Did failover occur?
- Which model epoch produced this artifact?

### 46.6 Context efficiency

The active prompt can shrink without losing the canonical trace or durable evidence references.

### 46.7 Composability

An external orchestrator can operate `pi-rs` without scraping terminal output.

### 46.8 Visual clarity

Long sessions remain skimmable through consistent semantic highlighting rather than heavy UI chrome.

---

## 47. Major Risks and Mitigations

### Pi compatibility scope creep

**Risk:** Attempting complete compatibility dominates development.

**Mitigation:** Versioned compatibility matrix; focus on public ecosystem surfaces.

### Trace growth

**Risk:** Reasoning and tool output consume large disk space.

**Mitigation:** Content-addressed blobs, compression, retention policy, optional raw capture.

### Sensitive logging

**Risk:** Traces preserve secrets.

**Mitigation:** Redaction-first storage, restrictive permissions, opt-in raw payloads.

### Incorrect reconstructed rationale

**Risk:** Inference sounds authoritative.

**Mitigation:** Strong provenance labels and separate visual treatment.

### Compaction loss

**Risk:** Important state disappears from active context.

**Mitigation:** Structured capsules, checkpoints, trace preservation, optional validation.

### Backup incompatibility

**Risk:** Backup lacks context size, tools, or modality.

**Mitigation:** Capability gate and context rebudgeting before takeover.

### MCP complexity

**Risk:** Interoperability expands core complexity.

**Mitigation:** Isolate behind adapters; lazy loading; support tested subsets first.

### Startup regression

**Risk:** Features gradually make the harness feel heavy.

**Mitigation:** Startup latency budgets, lazy initialization, dependency scrutiny, performance CI.

### TUI overdesign

**Risk:** "Modern" becomes visually noisy.

**Mitigation:** Semantic styling only; restrained chrome; collapse detail by default.

---

## 48. Foundational Invariants

Unless explicitly revised, the following should be treated as architectural rules:

1. The execution trace is distinct from model context.
2. Compaction never silently destroys canonical execution history.
3. Reasoning provenance is always explicit.
4. Hidden chain-of-thought is never claimed to have been recovered when it was not exposed.
5. Only one model is active in a normal execution role at a time.
6. Backup-model activation is fault recovery, not orchestration.
7. Potentially destructive tools are never blindly replayed across uncertain failure boundaries.
8. External evidence retains durable source attribution.
9. MCP is an interoperability boundary, not the internal agent architecture.
10. Pi compatibility is measured and versioned.
11. Domain-specific capabilities remain outside the core.
12. The default user experience requires little configuration.
13. Optional capability initialization must not block the editor from becoming usable.
14. Context should be optimized for useful information per unit inference cost, not maximal fill.
15. Visual design should encode semantics rather than decoration.
16. Rare and consequential runtime events should be prominent; routine execution should remain quiet.
17. Startup-path work and dependencies require stronger justification than lazy/on-demand work.
18. Session state belongs to the runtime, not to any particular model.

---

## 49. Design Mantras

### Product

> **Minimal core. Compatible ecosystem. Observable execution. Honest provenance. Recoverable state.**

### Context

> **Context is a cache, not the record.**

### External knowledge

> **Evidence should be attributable and rehydratable.**

### UX

> **Quiet by default. Rich when inspected. Semantic rather than decorative.**

### Performance

> **Instant before complete.**

### Engineering

> **Eager only for the critical path; lazy everywhere else.**

### Scope

> **If it defines how work is performed, it probably belongs outside the runtime. If it makes execution fundamentally more reliable, inspectable, compatible, or composable, it may belong inside.**

---

## 50. Long-Term Vision

A mature `pi-rs` session may look like:

```text
User
 |
 v
pi-rs
 |
 +-- Qwen local model
 |      |
 |      +-- exposed native reasoning
 |      +-- coding tools
 |      +-- RKB retrieval
 |      +-- MCP resources
 |
 +-- context reaches performance boundary
 |      |
 |      `-- semantic compaction
 |
 +-- local inference server crashes
 |      |
 |      `-- backup model takes over
 |
 +-- execution continues
 |
 +-- PR merged
 |      |
 |      `-- checkpoint/reset
 |
 `-- compact semantic state returned to user/orchestrator
```

Meanwhile the durable record preserves:

```text
user requests
model epochs
native reasoning that was exposed
provider summaries
declared rationales
reconstructed rationales
tool calls
tool results
external evidence
source provenance
context changes
compactions
checkpoints
failovers
final artifacts
```

The active model may have forgotten most of the intermediate detail.

The runtime has not.

That leads to the central architectural idea:

> **Working memory is disposable, evidence is rehydratable, execution is traceable, and the model itself is replaceable.**

`pi-rs` should remain a minimal coding harness at the surface while providing a more explicit, reliable runtime underneath.

That distinction should guide every major design decision.
