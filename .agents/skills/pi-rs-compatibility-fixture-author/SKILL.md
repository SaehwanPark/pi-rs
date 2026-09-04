---
name: pi-rs-compatibility-fixture-author
description: Implement or review Pi behavioral compatibility through versioned surface diagnostics and representative fixtures without weakening pi-rs core semantics.
---

# pi-rs Compatibility Fixture Author

## When to Use

Use this skill when adding, fixing, or evaluating compatibility for Pi skills, prompt templates, packages, sessions, themes, extension APIs, or lifecycle behavior.

Do not use it for provider compatibility, MCP protocol negotiation, undocumented Pi internals, or a Rust-native feature with no Pi-facing behavior.

## Required Inputs

- the compatibility surface and expected user-visible behavior
- the Pi behavior or version family being targeted
- documented upstream examples or an existing repository fixture
- the current status in `../../../COMPATIBILITY.md`
- the implementation boundary that should normalize the behavior

If in-scope public behavior has not been evaluated, mark it `Unknown`. Use `Experimental` only after a bounded implementation and fixture exist. Undocumented Pi internals and arbitrary internal imports remain `Unsupported`; do not infer support from one observed package.

## Workflow

### 1. Declare the target

State the surface, target behavior/version family, current status, intended status, and known exclusions. Compatibility is behavioral, not an internal source-porting exercise.

### 2. Capture behavior as fixtures

Add the smallest representative fixture under:

```text
tests/compat/{surface}/
```

Prefer:

- one minimal positive fixture
- one boundary or unsupported-case fixture
- one failure diagnostic when malformed or partially supported input matters

Add a public real-world fixture only when it is stable, legally reusable, and materially different from the synthetic case.

### 3. Implement behind the compatibility boundary

Normalize compatible behavior into Rust-native internal types. Keep Node, TypeScript, package, or Pi storage details out of the core agent loop. Do not weaken provenance, event, session, or tool-state semantics merely to mimic a less expressive format.

### 4. Report partial support honestly

Use only `Supported`, `Partial`, `Experimental`, `Unsupported`, or `Unknown`. Diagnostics should identify support per surface so one unsupported optional feature does not invalidate an otherwise usable package.

### 5. Test drift and round trips

Verify the user-visible behavior, unsupported diagnostics, and deterministic discovery. For session import/export, explicitly test metadata that cannot round-trip and preserve warnings rather than dropping it silently.

Use `references/fixture-checklist.md` for the surface-specific cases.

### 6. Reconcile compatibility state

Update `../../../COMPATIBILITY.md` and release notes only when the supported behavior or limitation changed. Keep fixture changes in the same slice as the compatibility fix.

## Outputs

- implementation isolated in the compatibility subsystem
- fixtures under `tests/compat/{surface}/`
- positive, boundary, and failure-path assertions proportional to the change
- optional `_workspace/changes/{change-id}/20_compatibility-report.md`
- updated compatibility status/limitations when warranted

## Validation

A compatibility claim is acceptable only when a fixture proves the behavior. Confirm that discovery is deterministic, relative resources still resolve, partial support is diagnosed per surface, and optional compatibility hosts remain lazy.

## References

- `references/fixture-checklist.md`
- `../../../COMPATIBILITY.md`
