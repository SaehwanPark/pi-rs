---
title: "pi-rs Proposal: A Minimal, Observable, Fault-Tolerant Agent Runtime"
author: "Sae-Hwan Park"
date: 2026-09-04
---

**Status:** Proposed  
**Working name:** `pi-rs`  
**Primary language:** Rust  
**Design lineage:** Heavily inspired by Pi  
**Project type:** Clean reimplementation, not source rewrite or fork

---

# 1. Executive Summary

`pi-rs` is a proposed minimal coding-agent runtime inspired by the philosophy and interaction model of Pi, reimplemented in Rust with several deliberate architectural extensions:

1. **Strong compatibility with the Pi ecosystem**, including skills, prompts, packages, and, where practical, TypeScript extensions.
2. **First-class execution observability**, preserving model-visible reasoning when exposed, tool calls, tool results, provider events, model transitions, context transformations, and other execution provenance.
3. **Explicit provenance for reasoning-like information**, distinguishing genuine model-emitted reasoning from provider summaries, declared rationales, and post-hoc reconstructions.
4. **Built-in context lifecycle management**, including proactive compaction, semantic phase boundaries, checkpoint/reset workflows, durable trace preservation, and rehydratable external context.
5. **MCP client and server support**, allowing `pi-rs` both to consume external capabilities and to serve as a composable worker within higher-level orchestration systems.
6. **Fault-tolerant model failover**, allowing a configured backup model to continue the same session when the primary model becomes unavailable.
7. **First-party integrations such as `rkb-rs`**, demonstrating provenance-aware external knowledge retrieval without coupling domain-specific knowledge systems into the core runtime.

The central thesis is:

> **A minimal agent runtime should keep its execution model simple while making state, provenance, context management, external capabilities, and failure recovery explicit runtime primitives.**

The project is not intended to become a large autonomous-development framework or multi-agent orchestration system.

Higher-level orchestration should remain outside the core and interact with `pi-rs` through stable APIs such as MCP and RPC.

---

# 2. Motivation

Pi demonstrates that a coding harness can remain powerful while maintaining a small conceptual core.

Its strongest ideas include:

- a relatively minimal agent loop;
- model/provider flexibility;
- extensibility rather than feature accumulation;
- plain and inspectable sessions;
- user-controlled workflows;
- packages, skills, prompts, and extensions as primary customization mechanisms.

`pi-rs` should preserve these ideas as closely as practical.

The motivation for a separate implementation is not simply to rewrite Pi in Rust. A language-only port would provide insufficient differentiation and create unnecessary ecosystem fragmentation.

Instead, `pi-rs` explores several runtime properties that are easier to implement coherently when they are foundational:

- execution traces that outlive working context;
- typed event provenance;
- context compaction as a runtime lifecycle;
- replay and branching;
- model failure recovery;
- durable external-context references;
- explicit interoperability boundaries;
- local-model-friendly performance and deployment.

The result should remain recognizably Pi-like while being optimized for long-running, inspectable, local or remote agent execution.

---

# 3. Project Thesis

A concise project description is:

> **`pi-rs` is a minimal, Pi-compatible agent runtime for local and remote coding models, designed around observable execution, bounded working context, fault tolerance, and composable orchestration.**

Four short principles summarize the project:

> **Minimal core.  
> Compatible ecosystem.  
> Observable execution.  
> Honest provenance.**

A fifth may be added as the architecture matures:

> **Recoverable execution.**

---

# 4. Goals

## 4.1 Primary goals

`pi-rs` should:

- preserve Pi's minimalist agent-runtime philosophy;
- offer a familiar workflow to existing Pi users;
- support major Pi package conventions;
- run efficiently as a native Rust application;
- support local and remote model providers;
- preserve all model/provider reasoning information that is genuinely exposed;
- record tool execution and relevant runtime events with strong provenance;
- allow users to inspect and replay execution;
- manage long-running context intelligently by default;
- remain usable without extensive configuration;
- expose and consume MCP capabilities;
- support graceful continuation through model/provider failures;
- make external knowledge retrievable without permanently bloating model context;
- keep orchestration policy outside the core.

## 4.2 Secondary goals

The project should also become useful as:

- a local-model experimentation harness;
- an agent-behavior research environment;
- a reproducible debugging environment;
- an execution substrate for higher-level orchestrators;
- a reference implementation for provenance-aware agent systems.

---

# 5. Non-Goals

The following should explicitly remain outside the initial core.

## 5.1 Multi-model orchestration

`pi-rs` should not automatically:

- ask multiple models the same question;
- vote among models;
- create manager/worker hierarchies;
- use judge models;
- select models based on task semantics;
- dynamically route work for quality or cost optimization.

These capabilities can be implemented through extensions or external orchestration.

Backup-model failover is different: only one model is active at a time and another takes over solely for execution continuity.

## 5.2 Hidden chain-of-thought extraction

`pi-rs` must never claim to recover internal reasoning that a provider or model does not expose.

If hidden reasoning never reaches the client, it cannot be recovered by the harness.

Instead, `pi-rs` may provide:

- provider-supplied reasoning summaries;
- explicitly requested compact rationales;
- post-hoc reconstructed rationale.

Their provenance must always remain distinguishable.

## 5.3 Built-in domain knowledge

Domain-specific systems such as `rkb-rs` should integrate deeply but remain external packages/services.

`pi-rs` should contain generic knowledge/context primitives rather than CMS-specific retrieval logic.

## 5.4 Large built-in workflow framework

The runtime should not accumulate:

- project-management systems;
- opinionated development methodologies;
- large agent-team abstractions;
- browser automation frameworks;
- deployment workflows;
- specialized research pipelines.

These belong in packages, MCP services, skills, or orchestrators.

---

# 6. Design Principles

## 6.1 Reimplementation, not rewrite

`pi-rs` should reproduce useful observable semantics rather than translate source architecture mechanically.

Compatibility should be measured through behavior and test fixtures.

Rust-native architectural improvements are encouraged where they preserve expected external behavior.

---

## 6.2 Context is a cache, not the record

This is a foundational architectural principle.

The model's current prompt is a temporary working set.

The canonical execution record is stored independently.

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

Compaction may destroy information from the model's active prompt.

It must not silently destroy the audit record.

---

## 6.3 One event model

Model activity, tools, compaction, failover, package actions, external retrieval, and replay should share one coherent event system.

Different subsystems should consume the same events rather than implement independent logging systems.

---

## 6.4 Mechanism in core, policy at the boundary

Core runtime primitives should be stable.

Policies should be replaceable or configurable.

For example:

```rust
trait ContextPolicy {
  fn evaluate(&self, state: &ContextState) -> ContextAction;
}
```

The runtime needs to understand `Compact`.

It does not need to hard-code every heuristic that leads to compaction.

---

## 6.5 Conservative automation

Automatic behavior should be predictable.

Examples:

- proactive compaction occurs only at safe boundaries;
- destructive resets require review;
- failover occurs only for defined availability/runtime failures;
- tools with uncertain side effects are not blindly replayed;
- external context is not injected without a reason.

---

## 6.6 Useful defaults over numerical tuning

Most users should never need to configure token thresholds, latency curves, or retry algorithms.

The default experience should behave sensibly.

Advanced tuning remains available when needed.

---

# 7. Proposed Architecture

A high-level architecture:

```text
+-------------------------------------------------------+
|                        pi-rs                          |
|                                                       |
|   +-------------+       +------------------------+    |
|   | Agent Loop  |<----->| Provider Abstraction   |    |
|   +------+------+       +-----------+------------+    |
|          |                          |                  |
|          +------------+-------------+                  |
|                       v                                |
|                +-------------+                         |
|                | Event Bus   |                         |
|                +------+------+                         |
|                       |                                |
|      +----------------+-------------------+            |
|      |                |                   |            |
|      v                v                   v            |
| Session Store   Context Engine      Trace/Replay       |
|      |                |                                |
|      |         +------+------+                         |
|      |         |             |                         |
|      |      Artifacts   External Context               |
|      |                       |                         |
|      +-----------------------+-------------------------+
|                              |
|                        MCP / extensions
+------------------------------+------------------------+
                               |
             +-----------------+------------------+
             |                 |                  |
           rkb-rs           GitHub             Browser
```

External orchestration operates in the opposite direction:

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

# 8. Core Runtime Components

A possible crate organization:

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

This is illustrative rather than prescriptive.

The key concern is separation of responsibilities.

---

# 9. Event and Trace Architecture

## 9.1 Agent event stream

Important runtime actions should produce typed events.

For example:

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
  ContextReduced,
  ContextCompactionStarted,
  ContextCompactionCompleted,
  CheckpointCreated,
  ModelRetry,
  ModelFailover,
  ExternalContextRetrieved,
  SessionEnded,
}
```

Each event should contain relevant identifiers such as:

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

where meaningful.

---

# 10. Reasoning and Transparency Model

The project should avoid treating every reasoning-like artifact as equivalent.

Four categories should be recognized.

## 10.1 Native reasoning

Actual reasoning content emitted by the model/provider.

```text
provenance = native
```

This should be preserved as faithfully as possible.

---

## 10.2 Provider reasoning summary

Some providers may expose a compressed or transformed description of hidden reasoning.

```text
provenance = provider_summary
```

This must not be described as raw chain-of-thought.

---

## 10.3 Declared rationale

The harness may optionally ask the model for concise structured justification around important actions.

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

This is intentionally generated explanation, not internal hidden reasoning.

```text
provenance = declared
```

---

## 10.4 Reconstructed rationale

A user may request post-hoc inference of why an agent likely made a decision based on:

- context visible at the time;
- tool calls;
- tool outputs;
- edits;
- later behavior.

```text
provenance = reconstructed
```

The UI must make clear that this is an inference.

For example:

```text
≈ RECONSTRUCTED RATIONALE
Not original model reasoning.
```

---

# 11. Trace Storage

Two logical representations are desirable.

## 11.1 Session state

A semantic session representation suitable for:

- resumption;
- branching;
- Pi compatibility;
- user inspection.

Example:

```text
session.jsonl
```

## 11.2 High-resolution trace

A detailed event journal:

```text
trace.jsonl
```

Potentially large payloads should be stored separately and referenced.

```text
artifacts/
blobs/
tool-results/
```

The trace should be append-oriented.

---

# 12. Replay

Replay should eventually become a first-class capability.

Examples:

```bash
pi-rs replay session.jsonl
pi-rs replay session.jsonl --tools
pi-rs replay session.jsonl --reasoning
pi-rs replay session.jsonl --timing
pi-rs replay session.jsonl --until event:381
```

Longer-term possibilities include:

- branching from an historical event;
- replaying with a different context policy;
- comparing two models from the same historical state;
- forensic visualization.

Replay must distinguish actual historical execution from newly generated continuations.

---

# 13. Pi Ecosystem Compatibility

Compatibility should be explicit and versioned rather than vaguely promised.

Possible levels:

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
| Arbitrary Node internals | Not guaranteed |

A compatibility command could expose:

```bash
pi-rs compat package ./some-package
```

and report:

```text
Pi compatibility

✓ skills
✓ prompts
✓ registerTool
✓ registerCommand
△ custom TUI widget
✗ internal pi module import
```

---

# 14. TypeScript Extension Compatibility

For maximum ecosystem compatibility, arbitrary TypeScript extensions should not initially be reimplemented as Rust plugins.

Instead, use a compatibility host.

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

- npm remains available;
- Node-specific packages continue to work;
- existing Pi extension code can often run unchanged;
- Rust remains isolated from JavaScript runtime complexity.

Later, `pi-rs` may additionally offer:

```text
TypeScript extensions -> compatibility path
Rust/WASM extensions  -> native path
```

The native path should complement rather than immediately replace Pi compatibility.

---

# 15. Context Lifecycle System

Context management should be built into the runtime.

The runtime should maintain three logically distinct layers:

```text
1. FORENSIC TRACE
   complete observable history

2. WORKING CONTEXT
   bounded prompt supplied to the model

3. DURABLE SEMANTIC STATE
   checkpoints, capsules, artifact references
```

The working context is allowed to forget.

The trace is not.

---

# 16. Context Management Levels

Four levels are proposed.

## L0: Eviction / replacement of oversized payloads

Large tool output should not necessarily live indefinitely in the prompt.

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

The original remains available.

---

## L1: Ordinary compaction

Old conversation content is summarized while recent context remains verbatim.

This handles normal context pressure.

---

## L2: Semantic phase compaction

Compaction may occur because a meaningful task phase is complete rather than because the context window is nearly full.

Examples:

- implementation complete;
- tests pass;
- debugging episode resolved;
- investigation concluded;
- PR ready.

This should preserve state relevant to the next phase.

---

## L3: Episode checkpoint/reset

After large semantic boundaries such as:

- PR merged;
- release completed;
- issue resolved;
- deployment completed;
- research episode finished;

the active working context may be replaced with a concise continuation capsule.

A checkpoint should remain available for later inspection.

---

# 17. Structured Context Capsules

Compaction should preferably generate structured semantic state rather than only free-form summaries.

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

The runtime can render this differently for different models without losing semantic structure.

---

# 18. Context Profiles

Default context management should be profile-driven.

```text
aggressive
balanced
relaxed
```

`balanced` should be the invisible default.

Users should select modes based on observed symptoms rather than machine specifications.

Example:

```text
Growing sessions become sluggish -> aggressive
Everything works comfortably     -> balanced
Compaction happens too often     -> relaxed
```

Advanced numerical thresholds may exist but should not be required.

Model context-window limits may automatically lower effective thresholds.

Thresholds should not be raised merely because a provider advertises an extremely large window.

---

# 19. Future Adaptive Context Policy

A later experimental feature may incorporate observed runtime characteristics:

```text
model context size
+
observed prefill latency
+
KV-cache characteristics
+
cache hits
+
hardware/runtime
         |
         v
dynamic effective context policy
```

For example, the runtime could discover that a local model's prefill latency rises sharply beyond 30k tokens and adjust compaction accordingly.

This should not be required for the first implementation.

Stable profile-based behavior is preferable initially.

---

# 20. MCP Client

MCP should be available as a first-party subsystem without contaminating the agent loop.

MCP tools should normalize into the same internal tool abstraction used by native tools and extensions.

```rust
Tool
  |- BuiltinTool
  |- ExtensionTool
  `- McpTool
```

The model should not care how the capability is implemented.

---

# 21. Lazy MCP Capability Exposure

Connecting many MCP servers can create hundreds of tool schemas.

Exposing all of them directly to a model is undesirable, especially for local models.

`pi-rs` should support capability filtering or lazy discovery.

Possible approaches:

```text
/mcp enable github
/mcp enable rkb
```

or meta-tools:

```text
mcp.search_capabilities(...)
mcp.call(...)
```

or skill-driven activation.

The exact mechanism should remain experimentally open.

The architectural requirement is:

> MCP integration must not imply permanent injection of every tool schema into every model prompt.

---

# 22. MCP Server

`pi-rs` should also be exposable as an MCP server.

This lets a higher-level orchestrator treat a coding session as one stateful worker.

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

The interface should expose useful agent-level semantics rather than every internal event.

---

# 23. Orchestration Boundary

`pi-rs` should deliberately avoid implementing a large multi-agent system internally.

Instead:

```text
                  ORCHESTRATOR
                       |
            +----------+----------+
            |          |          |
            v          v          v
         pi-rs A    pi-rs B    pi-rs C
         backend    frontend    reviewer
```

Each worker may independently be:

- stateful;
- inspectable;
- resumable;
- compactable;
- fault tolerant;
- auditable.

The orchestrator deals mainly in semantic state rather than entire worker histories.

---

# 24. External Context and Rehydration

External evidence should be represented as something stronger than copied text.

Conceptually:

```rust
struct ExternalContextRef {
  provider: String,
  resource_id: String,
  citation: Option<String>,
  provenance: Provenance,
}
```

Working context may contain a temporary rendering.

The durable session contains the reference.

This creates three possible states for information:

```text
resident
compacted
rehydratable
```

This is superior to the traditional binary model:

```text
remembered
forgotten
```

---

# 25. `rkb-rs` First-Party Integration

`rkb-rs` should remain an independent project.

It should not become a dependency of the `pi-rs` core.

Instead, it should serve as an official example of provenance-aware external knowledge integration.

Possible structure:

```text
pi-rs core
  `- generic external-context primitives

pi-rs-rkb
  `- official integration

rkb-rs
  `- independent knowledge system
```

The integration may provide:

- MCP discovery/setup;
- skills explaining when RKB should be used;
- citation-aware rendering;
- preservation of RKB resource IDs;
- context rehydration hooks;
- compaction-aware evidence references.

A retrieved RKB document can therefore leave the active prompt while preserving:

```text
decision
citation
resource ID
source provenance
```

and be retrieved again if needed.

---

# 26. Model Failover

Model failover should be a core reliability feature.

It must remain explicitly distinct from model orchestration.

Configuration might be:

```toml
[model]
primary = "local/qwen"
backup = "openai/gpt"
```

Only one model is active at any moment.

---

# 27. Failover Semantics

The runtime should follow a deterministic sequence:

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

Retry precedes failover.

---

# 28. Eligible Failover Conditions

Automatic failover may apply to:

- provider outage;
- local server unavailable;
- connection refusal;
- repeated transport failures;
- timeout after configured retries;
- provider 5xx conditions;
- persistent rate limiting;
- unrecoverable stream interruption;
- model endpoint disappearance;
- repeated protocol corruption attributable to the provider/model.

Automatic failover should normally not occur because:

- the answer is weak;
- tests fail;
- the model chooses a poor implementation;
- the model disagrees with the user;
- the runtime judges the model "confused."

Those belong to quality control or orchestration.

---

# 29. Mid-Turn Failover

A backup model must continue from actual committed session state.

Suppose:

```text
primary:
  read file
  edit file
  run test
  [connection lost]
```

The backup should receive the observable session state after those successful operations.

The entire turn must not simply be replayed.

---

# 30. Tool Transaction State

Every tool call should have explicit lifecycle state:

```text
Requested
Started
Succeeded
Failed
Unknown
```

Tool-call IDs should be durable.

If the runtime does not know whether a side-effecting tool completed, it must not blindly replay it after failover.

Read-only operations may be safely retried more often.

Side effects should be inspected or reconciled.

---

# 31. Backup Compatibility Gate

A backup model may differ from the primary.

The runtime should inspect capabilities such as:

```text
                     primary   backup
text                    yes      yes
images                  yes      no
tool calling            yes      yes
context window          128k     32k
reasoning exposed       yes      no
```

A failover may therefore require context transformation.

If the current working context exceeds the backup's capacity:

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

The context subsystem therefore directly supports runtime fault tolerance.

---

# 32. Failover Provenance

Model changes should be visible in the trace.

Example:

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

A session can therefore contain multiple execution epochs:

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

---

# 33. Failover Recovery Policy

The runtime should avoid automatic model ping-pong.

Initial behavior should be conservative:

> After automatic failover, the backup remains active until the user explicitly switches back.

Later releases may optionally reassess primary availability at safe turn boundaries.

Manual commands may include:

```text
/failover
/model-primary
/model-backup
```

Exact naming can be refined.

---

# 34. Local Model Support

Local-model usage should be a first-class design consideration rather than an afterthought.

Relevant runtimes may include:

- llama.cpp-compatible endpoints;
- OpenAI-compatible local servers;
- other extensible provider adapters.

The runtime should handle:

- long prefill latency;
- variable context windows;
- exposed reasoning tokens;
- model-specific tool-call formats;
- local server crashes;
- quantization-dependent performance;
- limited context compared with cloud models.

No one local runtime should be mandatory.

---

# 35. Security and Privacy

Observability increases both usefulness and risk.

Trace data may contain:

- source code;
- `.env` contents;
- credentials accidentally returned by tools;
- database connection strings;
- proprietary documents;
- personal information;
- provider metadata;
- model reasoning that repeats sensitive values.

A redaction layer should therefore exist before durable logging.

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

Raw provider payload storage should be optional and disabled by default.

Potential configuration:

```toml
[trace]
raw_provider_payloads = false
```

Sensitive local state should use restrictive filesystem permissions.

---

# 36. Project Trust

Project-local configuration should not silently gain powerful capabilities in an untrusted repository.

Trust-sensitive behavior may include:

- enabling MCP servers;
- launching subprocesses;
- modifying context policies;
- activating extensions;
- writing checkpoints outside approved locations.

Global and project-level configuration should have clearly defined precedence.

---

# 37. Configuration Philosophy

The default configuration should be extremely small.

A normal user may need nothing beyond:

```toml
[model]
primary = "local/qwen"
backup = "openai/gpt"

[context]
mode = "balanced"
```

Everything else should have sensible defaults.

Advanced configuration may expose:

```text
retry behavior
trace retention
redaction
context thresholds
MCP discovery
extension hosts
checkpoint storage
```

but these should remain optional.

---

# 38. UX Principles

The interface should remain recognizably simple.

Normal execution should not constantly expose internal machinery.

Relevant status should appear only when useful.

Examples:

```text
Context compacted: 31k -> 12k
```

```text
Primary unavailable; continuing with backup model.
```

```text
RKB evidence compacted; citations remain rehydratable.
```

Detailed inspection should be user-requested:

```text
/context-stats
/trace
/checkpoints
/model-status
/mcp
```

---

# 39. Core vs First-Party vs External

A tentative feature boundary:

| Capability | Location |
|---|---|
| Agent loop | Core |
| Provider abstraction | Core |
| Tool execution | Core |
| Event store | Core |
| Session persistence | Core |
| Reasoning provenance | Core |
| Context lifecycle | Core |
| Model failover | Core |
| MCP protocol support | First-party core subsystem |
| Pi package compatibility | Core/compat subsystem |
| Node extension host | First-party |
| RKB integration | First-party extension |
| Browser integration | Extension/MCP |
| Multi-agent orchestration | External/extension |
| Model voting/judging | External/extension |
| Domain workflows | Extensions/skills |

The distinction should remain strict enough to prevent uncontrolled core growth.

---

# 40. Testing Strategy

`pi-rs` should be test-driven around externally visible semantics.

## 40.1 Compatibility fixtures

Create fixtures representing:

- Pi skills;
- package manifests;
- prompt templates;
- session files;
- common extension APIs.

CI should report compatibility regressions.

---

## 40.2 Provider simulation

A fake deterministic provider should support scenarios such as:

- successful streaming;
- exposed reasoning;
- malformed reasoning;
- interrupted stream;
- timeout;
- 429;
- 500;
- context overflow;
- malformed tool call.

This enables reliable testing without depending on live APIs.

---

## 40.3 Failover simulation

Tests should cover:

```text
primary failure before output
primary failure after reasoning
primary failure after read-only tool
primary failure after write
uncertain tool completion
smaller backup context
backup lacks image capability
backup also fails
```

---

## 40.4 Compaction invariants

Tests should verify that compaction preserves:

- active user objective;
- explicit constraints;
- unresolved tasks;
- relevant artifacts;
- pending actions.

The trace must retain original events after working-context compaction.

---

## 40.5 Replay invariants

Given a historical session, replay should reproduce:

- event ordering;
- model epochs;
- tool execution state;
- context transformations;
- provenance labels.

---

# 41. Observability for Development

The same event architecture should support developer diagnostics.

Potential outputs:

```bash
pi-rs trace
pi-rs trace --json
pi-rs trace --tools
pi-rs trace --reasoning
pi-rs doctor
```

Optional OpenTelemetry export may be added without making OpenTelemetry a conceptual dependency of the runtime.

---

# 42. Development Roadmap

## Phase 0: Architecture and compatibility specification

Deliverables:

- architecture document;
- project principles;
- Pi compatibility matrix;
- event schema;
- provider abstraction;
- session format;
- explicit non-goals.

Avoid premature feature implementation.

---

## Phase 1: Minimal agent

Implement:

- CLI;
- provider interface;
- one or two providers;
- streaming assistant output;
- basic tools;
- session persistence;
- minimal TUI.

Goal:

> A small usable coding agent exists before advanced features are added.

---

## Phase 2: Trace and provenance

Implement:

- event bus;
- detailed trace storage;
- reasoning provenance;
- tool lifecycle events;
- execution epochs;
- basic trace inspection.

This should happen early because later systems depend on reliable events.

---

## Phase 3: Pi compatibility foundation

Implement:

- skills;
- prompts;
- package discovery;
- package configuration;
- session import/export where practical;
- compatibility fixtures.

Do not attempt complete extension compatibility immediately.

---

## Phase 4: Context lifecycle

Port and generalize the design lessons from `local-context-manager`:

- context statistics;
- `balanced`/`aggressive`/`relaxed`;
- proactive safe-boundary compaction;
- oversized tool-result reduction;
- semantic phase compaction;
- checkpoint/reset;
- structured capsules.

---

## Phase 5: Model failover

Implement:

- failure classification;
- retry policy;
- primary/backup configuration;
- execution epochs;
- capability checking;
- failover-aware context rebudgeting;
- uncertain-side-effect safeguards.

---

## Phase 6: MCP client

Implement:

- MCP transport abstraction;
- tool discovery;
- capability filtering;
- tool normalization;
- protocol negotiation;
- local server lifecycle where appropriate.

Use `rkb-rs` as an early integration fixture.

---

## Phase 7: `rkb-rs` first-party integration

Implement:

- package/skill;
- MCP setup;
- provenance-preserving results;
- durable external references;
- evidence-aware compaction;
- context rehydration.

This phase should help refine generic external-context abstractions.

---

## Phase 8: Extension compatibility host

Implement Node/TypeScript compatibility for high-value Pi extension APIs:

- tool registration;
- commands;
- lifecycle events;
- context hooks;
- selected UI facilities.

Expand based on real package compatibility rather than hypothetical completeness.

---

## Phase 9: MCP server / worker mode

Expose `pi-rs` itself as a reusable worker.

Implement:

- start/resume/cancel semantics;
- session resources;
- artifacts;
- summaries;
- trace access;
- long-running task behavior.

---

## Phase 10: Replay and research tooling

Add:

- visual replay;
- event navigation;
- historical context reconstruction;
- branch-from-event;
- model comparison experiments;
- reasoning/provenance analysis.

---

# 43. Initial MVP Boundary

The first public MVP should resist implementing the entire vision.

A strong MVP could contain:

- native Rust binary;
- one local and one cloud-compatible provider path;
- basic coding tools;
- persistent sessions;
- event trace;
- native reasoning preservation;
- Pi skills/prompts;
- basic context management;
- primary + backup model;
- MCP client;
- simple Pi-like interaction.

A reasonable rule:

> Do not delay the first usable agent for full Pi extension compatibility, sophisticated replay, or autonomous context adaptation.

---

# 44. Success Criteria

The project succeeds if:

## Compatibility

A Pi user can move common skills and packages to `pi-rs` with little or no modification.

## Minimalism

A new user can run the harness without learning an orchestration framework or context-tuning system.

## Local-model robustness

Long-running sessions remain usable despite:

- slower prefill;
- finite context;
- local server instability;
- experimental model behavior.

## Auditability

A user can answer:

- Which model generated this?
- What tools had run?
- What evidence had the model seen?
- Was this reasoning native or reconstructed?
- Did compaction occur before or after this action?
- Was a backup model involved?

## Context efficiency

Working context can shrink without destroying the canonical trace or losing durable references to important evidence.

## Composability

An external orchestrator can operate `pi-rs` without scraping terminal output.

---

# 45. Risks

## 45.1 Pi compatibility scope creep

Attempting perfect compatibility with every Pi internal API could dominate development.

**Mitigation:** maintain a versioned compatibility matrix and prioritize public ecosystem surfaces.

---

## 45.2 Trace storage growth

Native reasoning and tool outputs can become enormous.

**Mitigation:** content-addressed blobs, compression, retention policy, configurable payload storage.

---

## 45.3 Security through excessive logging

Observability can accidentally preserve secrets.

**Mitigation:** default redaction, optional raw payload capture, restrictive permissions.

---

## 45.4 Incorrect reconstructed rationale

Post-hoc rationale can sound authoritative despite being inferred.

**Mitigation:** strong provenance labels and separate rendering.

---

## 45.5 Compaction information loss

Poor summaries can silently eliminate important state.

**Mitigation:** structured capsules, checkpoint retention, replayable original trace, validation.

---

## 45.6 Backup semantic mismatch

A backup model may lack tools, modalities, or adequate context.

**Mitigation:** capability checks and context rebudgeting before takeover.

---

## 45.7 MCP complexity

Trying to support every transport/spec variation immediately could enlarge the core.

**Mitigation:** isolate MCP behind a protocol adapter and support a well-tested subset first.

---

# 46. Open Design Questions

Several decisions should remain deliberately unresolved until implementation reveals the correct boundary.

### Session compatibility

Should `pi-rs` use Pi's session format natively or provide import/export adapters around a stronger internal format?

### Extension host

Should the Node host communicate through JSON-RPC, MessagePack, or another typed transport?

### Artifact store

Should large payloads use:

- filesystem blobs;
- SQLite;
- content-addressed storage;
- a hybrid?

### Context capsules

Should capsules have one stable schema or a small versioned set by task class?

### MCP tool discovery

Should capability activation be:

- explicit;
- model-driven;
- skill-driven;
- usage-based?

### Trace privacy

What should the default retention and redaction policies be?

These should be answered through prototypes and fixtures rather than architectural speculation alone.

---

# 47. Project Identity

The project should avoid positioning itself primarily as:

> “Pi rewritten in Rust.”

That description understates both the purpose and the differentiation.

A stronger description is:

> **`pi-rs` is a Pi-inspired, Pi-compatible agent runtime built in Rust for observable, context-efficient, and fault-tolerant coding sessions across local and remote models.**

An even shorter tagline could be:

> **Minimal agents. Observable execution. Recoverable state.**

---

# 48. Long-Term Vision

If the architecture works as intended, a future execution might look like this:

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

Meanwhile, the durable record preserves:

```text
user requests
model epochs
native reasoning that was exposed
declared rationales
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

The active model may have forgotten most of the intermediate details.

The runtime has not.

This creates an agent architecture in which:

> **working memory is disposable, evidence is rehydratable, execution is traceable, and the model itself is replaceable.**

That is the architectural idea at the center of `pi-rs`.

---

# 49. Proposed Foundational Rules

The following rules should be treated as invariants unless explicitly revised:

1. **The execution trace is distinct from model context.**
2. **Compaction never silently destroys the canonical execution history.**
3. **Reasoning provenance is always explicit.**
4. **Hidden chain-of-thought is never claimed to have been recovered when it was not exposed.**
5. **Only one model is active within a normal `pi-rs` execution role at a time.**
6. **Backup-model activation is fault recovery, not orchestration.**
7. **Potentially destructive tool actions are never blindly replayed across uncertain failure boundaries.**
8. **External knowledge remains attributable to durable sources.**
9. **MCP is an interoperability boundary, not the internal agent architecture.**
10. **Pi compatibility is measured and versioned rather than asserted vaguely.**
11. **Domain-specific capabilities remain outside the core.**
12. **The normal user experience should require very little configuration.**

---

# 50. Conclusion

`pi-rs` should preserve what makes Pi attractive: a small agentic abstraction, flexible models, extensibility, and user control.

Its main innovation should not be Rust itself.

Rust is the substrate that makes it practical to build a runtime where:

- execution state is strongly typed;
- events are durable;
- context has an explicit lifecycle;
- tool side effects can be tracked;
- external resources can be referenced rather than copied indefinitely;
- model failures can be recovered from;
- local models can be treated as serious first-class execution engines;
- higher-level systems can compose workers without embedding orchestration policy into them.

The intended result is not a larger Pi.

It is a **more explicit runtime underneath a similarly minimal user-facing agent**.

That distinction should guide every major design decision.

The long-term standard for adding a feature should therefore be:

> **Does this make the agent runtime fundamentally more reliable, inspectable, compatible, or composable?**

If yes, it may belong in the runtime.

If it defines how a user or organization chooses to perform work, it probably belongs in an extension, skill, MCP service, or higher-level orchestrator.
