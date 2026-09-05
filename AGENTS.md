# AGENTS.md

## Purpose

This repository is the implementation of `pi-rs`, a minimal Pi-inspired coding-agent runtime in Rust.

Agents working here should optimize for:

- architectural clarity;
- correctness;
- low latency;
- predictable behavior;
- compatibility;
- explicit provenance;
- testability;
- restrained scope.

Read these before substantial changes:

1. `docs/PROJECT_DESIGN_CANONICAL.md`
2. `ARCHITECTURE.md`
3. `COMPATIBILITY.md`
4. `ROADMAP.md`

## Core project rules

1. Preserve Pi-like minimalism at the user-facing layer.
2. Do not add workflow policy to core unless it is a fundamental runtime concern.
3. Keep canonical execution trace separate from model-visible context.
4. Never conflate native reasoning, provider summaries, declared rationale, and reconstructed rationale.
5. Never claim hidden chain-of-thought was recovered unless it was actually exposed.
6. Only one model is active in a normal execution role at a time.
7. Backup model behavior is fault recovery, not orchestration.
8. Never blindly replay uncertain mutating tool operations.
9. Keep MCP behind an adapter boundary.
10. Keep domain-specific knowledge outside core.
11. Prefer lazy initialization for optional systems.
12. Protect startup latency aggressively.
13. Prefer explicit typed states over implicit booleans/string conventions.
14. Keep compatibility behavior covered by fixtures.
15. Avoid adding dependencies without a concrete reason.

## Orchestrator Guidelines

- You are the primary orchestrator running on a model from internal or external providers. When you are a model from an external provider, you should be careful about the usage limit. See the External Provider Usage Limits section below.
- Decompose complex implementation tasks, difficult refactors, and test generation into bounded subtasks.
- Spawn `worker` (see below for which models to use) using the `subagent` tool for deep code generation and verification.
- Run multiple independent workers in parallel where feasible.
- Synthesize worker outcomes, review diffs, and report final status to the user.
- If code is reviewed before opening PR, skip the review step and open the PR directly.

## Available AI Subscription Identification

- Orchestrator can spawn subagents (workers, reviewers, etc.) from the external providers `openai-codex` and Antigravity (both from subscription) as well as internal provider `local-vulcan`.
- Subagent models to use: `gpt-5.6-luna` from `openai-codex` provider (supported by this harness), Gemini 3.8 Fresh from Antigravity (note: this model should be used by `agy` headless mode), or `qwen3.8-flash` from `local-vulcan` internal provider (supported by this harness).
- Subagent models priority (in spawning): `gpt-5.6-luna`, Gemini 3.8 Fresh, `qwen3.8-flash`. (higher priority = earlier in the list)

## External Provider Usage Limits

- External providers set 5-hour and weekly usage limits.
- Therefore, you should carefully monitor them. When limits are approaching, you should gracefully wrap up ongoing tasks or wait until the limit is reset.
- Refer to [codexbar document](docs/codexbar.md) for details.
- Note internal providers are limitless because they run locally on this machine.

## Coding style

Use idiomatic stable Rust.

Preferred properties:

- use **2 spaces of tabsize** throughout;
- small focused modules;
- explicit types for domain state;
- typed `Result` errors;
- no `unsafe` without an architecture-level justification;
- topologically readable control flow;
- avoid hidden global state;
- avoid macros when ordinary code is clearer;
- keep I/O at boundaries;
- keep transformation logic testable and deterministic;
- avoid unnecessary cloning in hot paths;
- avoid premature async/concurrency complexity.

## Function organization

Within a module, prefer functions ordered so readers encounter higher-level/public behavior before lower-level helpers only when that improves readability.

For dependency-heavy logic, keep call relationships easy to follow and avoid circular module ownership.

## Performance rules

Treat performance as product behavior.

For code on the startup path:

- avoid network calls;
- avoid full directory scans;
- avoid deep session hydration;
- avoid starting Node;
- avoid eager MCP connection;
- avoid opening heavy indexes;
- avoid unnecessary allocations.

Before adding startup-path work, ask:

> Must this happen before the user can type or submit the first prompt?

If not, defer it.

Benchmark changes that affect:

- startup;
- TUI rendering;
- session resume;
- context reconstruction;
- package discovery;
- MCP first use;
- extension-host startup.

## Event rules

Important state transitions should emit typed events.

Do not add ad hoc logs when a semantic event is appropriate.

New event types must document:

- why they exist;
- ordering expectations;
- persistence requirements;
- replay implications;
- UI relevance.

## Session rules

Session state belongs to the runtime.

Do not make session correctness depend on one provider's representation.

Do not require full trace hydration to resume a session.

Prefer:

```text
latest checkpoint
+
post-checkpoint active events
```

for fast restoration.

## Tool rules

Every tool call must have a durable ID and lifecycle state.

Mutating operations must be explicit.

When completion is uncertain:

```text
Unknown
```

is a valid state.

Do not silently coerce unknown into failure or success.

## Context rules

The context engine may reduce model-visible state but must preserve canonical history.

Compaction should preserve:

- active objective;
- explicit constraints;
- important decisions;
- unresolved work;
- important artifacts;
- next actions.

Do not let context-window size alone determine policy.

Large supported windows do not imply that large working contexts are desirable.

## Reasoning provenance

Use explicit provenance.

Conceptually:

```rust
enum ReasoningProvenance {
  Native,
  ProviderSummary,
  Declared,
  Reconstructed,
}
```

Rendering and serialization must preserve the distinction.

## Failover rules

Automatic failover is allowed only for defined availability/runtime failures.

Do not fail over because:

- answer quality is low;
- tests fail;
- implementation is poor;
- model seems confused.

Retry transient failures before failover.

Before takeover:

- check backup capabilities;
- rebudget context if necessary;
- preserve committed tool state;
- record a model epoch transition.

After automatic failover, remain on backup until explicit user action unless the architecture changes later.

## MCP rules

MCP should be lazy.

Do not:

- connect every server at startup;
- inject every tool schema into every prompt;
- let MCP-specific details leak into the core agent loop.

Normalize MCP tools into the internal tool abstraction.

## Pi compatibility rules

Compatibility is behavioral and versioned.

Do not implement undocumented Pi internals merely to claim parity.

Prioritize:

1. skills;
2. prompts;
3. packages;
4. sessions;
5. extension tools/commands;
6. lifecycle/UI compatibility.

Add or update fixtures for every compatibility fix.

## TUI rules

Visual design should be modern but restrained.

Prefer:

- semantic syntax highlighting;
- operation/argument/path distinction;
- provenance-aware reasoning labels;
- subtle status;
- collapsible detail;
- clear rare-event emphasis.

Avoid:

- permanent dashboards;
- excessive borders;
- animation-heavy feedback;
- large always-visible token gauges;
- decorative color without semantic meaning.

## Security rules

Trace data may contain secrets.

All durable trace output should pass through redaction policy.

Raw provider payload capture must remain opt-in.

Respect project trust before:

- launching project-defined MCP servers;
- loading project-defined extensions;
- applying sensitive project-local configuration;
- writing outside approved state locations.

## Testing expectations

For any non-trivial change:

- add unit tests for pure logic;
- add integration tests for boundary behavior;
- add compatibility fixtures when applicable;
- add failure-path tests;
- add performance tests if startup/render/resume paths change.

For failover changes, include at least one mid-turn failure case.

For context changes, verify canonical trace preservation.

For event changes, verify deterministic ordering.

## Dependency policy

A new dependency requires:

- concrete need;
- maintained ecosystem status;
- acceptable binary/startup cost;
- no simpler standard-library/local alternative;
- documented reason when non-obvious.

Dependencies on the startup path deserve extra scrutiny.

## Scope discipline

If a proposed feature answers:

> How should users organize or orchestrate work?

it probably belongs in an extension, skill, MCP service, or external orchestrator.

If it answers:

> What does the runtime need to remain reliable, inspectable, compatible, context-efficient, or composable?

it may belong in core.

## Roadmap discipline

Update `ROADMAP.md` when:

- starting a tracked item;
- completing a tracked item;
- adding a new implementation stage;
- changing a stage gate;
- discovering a blocking architecture issue.

Do not mark an entire phase complete until its stage gate is satisfied.

## Definition of done

A change is not done merely because it compiles.

It should also satisfy, where applicable:

- tests;
- compatibility;
- trace/provenance correctness;
- performance expectations;
- failure behavior;
- documentation;
- roadmap state.

Prefer a smaller complete vertical slice over a broad incomplete subsystem.
