---
name: pi-rs-change-orchestrator
description: Plan and deliver non-trivial pi-rs changes as bounded roadmap-aligned slices with explicit routing, ownership, review, and verification.
---

# pi-rs Change Orchestrator

## When to Use

Use this skill for a non-trivial feature, refactor, compatibility change, or runtime behavior change in `pi-rs`.

Do not use it for a typo, isolated prose correction, or mechanical edit that can be verified directly. The existence of specialists does not justify delegation by itself.

## Required Inputs

- the user request and acceptance criteria
- the repository state and, when work already exists, the current diff
- `../../../docs/PROJECT_DESIGN_CANONICAL.md`, `../../../ARCHITECTURE.md`, `../../../COMPATIBILITY.md`, and `../../../ROADMAP.md`
- any issue, fixture, provider behavior, or upstream Pi behavior that constrains the change

When requirements are incomplete, inspect the repository and make the narrowest reversible assumption. Ask the user when the choice would change public behavior, compatibility, durable storage, or security.

## Workflow

### 1. Bound the slice

1. Locate the request in `../../../ROADMAP.md`, or state why it is untracked.
2. Classify the capability as core, first-party subsystem, compatibility layer, extension/skill, or external orchestration.
3. Reject workflow policy from core unless it is necessary for runtime reliability, inspectability, compatibility, context efficiency, or composability.
4. Define one vertical slice with observable behavior, owned paths, tests, and explicit non-goals.
5. Record the brief in-thread for ordinary work. For resumable or multi-agent work, write `_workspace/changes/{change-id}/00_change-brief.md`.

### 2. Route only where specialization pays

- Use `pi-rs-core-contract-designer` when the change defines events, identities, provider failures, tool lifecycle, provenance, model epochs, sessions, traces, context policies/capsules, failover state, trust, or redaction contracts.
- Use `pi-rs-compatibility-fixture-author` when behavior affects Pi skills, prompts, packages, sessions, themes, or extension APIs.
- Independently require `pi-rs-invariant-reviewer` after non-trivial runtime changes or changes on startup, persistence, failover, context, or security paths.
- Keep implementation with one writer per checkout. A delegated specialist is either a read-only advisor or the sole writer for explicitly assigned paths; the change owner synthesizes and accepts without duplicating those writes. Parallelize only read-heavy investigation or work with non-overlapping ownership or isolated worktrees.

### 3. Implement the smallest complete slice

1. Preserve explicit typed states and keep I/O at boundaries.
2. Add unit tests for deterministic transformations and boundary/failure tests for runtime behavior.
3. Add compatibility fixtures when user-visible Pi behavior changes.
4. Avoid new dependencies unless the standard library or a small local implementation is insufficient.
5. Keep optional systems off the startup path and lazy by default.

### 4. Review and revise

Run the invariant review against the request, change brief, and diff. Review status is one of:

- `pass`: no blocking finding
- `fix`: bounded corrections are required
- `redo`: the design violates a foundational boundary

Allow one focused revision pass. If a second pass still finds a blocking architecture issue, stop and present the decision to the user rather than widening scope silently.

### 5. Verify and reconcile

Run the checks relevant to the changed files. For a full change, the default Rust sequence is:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

Also verify any compatibility fixtures, failure paths, serialization round trips, or performance baselines implicated by the slice. Update `../../../ROADMAP.md` when an item starts, completes, changes gate, or becomes blocked. Update architecture or compatibility docs only when their stated contract changed.

## Outputs

- bounded change brief, in-thread or `_workspace/changes/{change-id}/00_change-brief.md`
- implementation and proportional tests
- optional specialist notes under the same change directory
- validation evidence, in-thread or `_workspace/changes/{change-id}/90_validation-report.md`
- roadmap/documentation updates when project state changed

## Validation

A completed slice must show:

- why the capability belongs at its chosen boundary
- which invariant-sensitive states and failure paths were tested
- exact commands run and their outcomes
- any residual risk, unsupported behavior, or deferred work
- no overlapping concurrent writes in one checkout

See `../../../docs/harness/pi-rs-development/team-spec.md` for routing, handoff, and failure policy.
