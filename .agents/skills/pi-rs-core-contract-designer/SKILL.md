---
name: pi-rs-core-contract-designer
description: Design and implement pi-rs runtime contracts with typed state, explicit provenance, deterministic events, and testable failure semantics.
---

# pi-rs Core Contract Designer

## When to Use

Use this skill when defining or changing core contracts for providers, events, IDs, sessions, traces, tools, context, model epochs, failover, redaction, or project trust.

Do not use it for ordinary leaf implementation that does not change a runtime contract, or for domain workflow policy that belongs outside core.

## Required Inputs

- the requested behavior and its failure cases
- the relevant roadmap item and stage gate
- canonical design and architecture invariants
- callers, persistence formats, and UI/adapter consumers affected by the contract
- compatibility or migration constraints, if data is durable or public

## Workflow

### 1. Establish ownership and boundary

Name the module that owns the state and the adapters that may observe it. Confirm that the contract is fundamental runtime machinery rather than provider-, MCP-, TUI-, or workflow-specific policy.

### 2. Specify behavior before representation

Write a compact table covering:

- valid states and transitions
- stable identity and ordering requirements
- error categories and retry/failover eligibility
- persistence and replay implications
- model-visible versus canonical representations
- redaction, trust, and UI relevance

For a complex or resumable change, save it as `_workspace/changes/{change-id}/10_contract-notes.md`.

### 3. Define typed contracts

Prefer enums and newtypes over booleans and string conventions. Preserve distinctions such as:

- canonical trace versus derived context
- `Requested`, `Started`, `Succeeded`, `Failed`, and `Unknown`
- `Native`, `ProviderSummary`, `Declared`, and `Reconstructed`
- semantic/quality errors versus availability/runtime errors

Do not introduce a state that downstream code must infer from missing data.

### 4. Keep effects at boundaries

Keep state transitions and transformations deterministic where possible. Isolate clocks, IDs, serialization, filesystem access, network access, and provider payload parsing behind narrow boundaries so pure behavior can be unit tested.

### 5. Implement tests as the contract

Cover:

- every valid transition and rejected transition
- serialization round trips and version behavior when durable
- deterministic ordering when events are emitted
- canonical history preservation when context changes
- uncertain mutating tool completion
- mid-turn failover when failover state is involved
- redaction before durable output when sensitive data is involved

Use the detailed prompts in `references/contract-checklist.md` to avoid omissions.

### 6. Reconcile documentation

Document why a new event exists, ordering expectations, persistence requirements, replay implications, and UI relevance. Update `../../../ARCHITECTURE.md`, `../../../COMPATIBILITY.md`, or `../../../ROADMAP.md` only when their contract or tracked state actually changes.

## Outputs

- focused Rust types and behavior at the owning boundary
- unit and boundary/failure tests
- optional `_workspace/changes/{change-id}/10_contract-notes.md`
- concise notes on persistence, replay, provenance, and migration impact

## Validation

Run formatting, clippy, and relevant tests. Treat compilation alone as insufficient. Confirm that no adapter-specific representation leaked into the core contract and that unknown or unavailable information was not silently coerced into success, failure, or native provenance.

## References

- `references/contract-checklist.md`
- `../../../docs/PROJECT_DESIGN_CANONICAL.md`
- `../../../ARCHITECTURE.md`
