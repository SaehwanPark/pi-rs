---
name: pi-rs-invariant-reviewer
description: Review pi-rs plans and diffs for violations of runtime, provenance, failover, compatibility, security, and startup invariants.
---

# pi-rs Invariant Reviewer

## When to Use

Use this skill for read-only review of a non-trivial plan or diff that touches runtime contracts, events, persistence, context, tools, providers, failover, MCP, compatibility, TUI semantics, security, or startup.

Do not invoke it for trivial edits. This role reviews and reports; it does not become a second implementation owner.

## Required Inputs

- original request and acceptance criteria
- change brief or stated scope
- current diff or proposed design
- available test evidence; if none exists yet, an explicit note that final verification is pending
- canonical design, architecture, compatibility, and roadmap state

## Workflow

### 1. Reconstruct intent

State the promised behavior, explicit non-goals, changed surfaces, and stage gate. Distinguish requirements from reviewer inference.

### 2. Inspect changed paths and consumers

Read the implementation, tests, serializers, adapters, and call sites implicated by the change. Review the actual boundary, not only the edited function.

### 3. Apply the targeted invariant matrix

Read `references/invariant-matrix.md` and apply only relevant sections. Prioritize concrete correctness risks over stylistic preferences.

### 4. Verify evidence

For each finding, cite the file and line or exact design statement, describe the failure scenario, and name the smallest correction. Do not claim a failure from speculation alone; label unresolved questions separately.

### 5. Produce a bounded verdict

Use:

- `pass`: no blocking findings
- `fix`: one or more bounded corrections are required
- `redo`: the proposed boundary contradicts a foundational invariant

Rank findings:

- `P0`: data loss, secret exposure, unsafe side effect, or unusable runtime
- `P1`: incorrect core semantics, provenance, replay, failover, or compatibility claim
- `P2`: meaningful robustness, performance, or maintainability defect
- `P3`: optional improvement; never blocks approval by itself

If durable handoff is needed, write `_workspace/changes/{change-id}/80_invariant-review.md`.

## Outputs

```markdown
# Invariant Review

Verdict: pass | fix | redo

## Findings
### P1 — Short title
- Evidence: `path:line`
- Failure scenario:
- Smallest correction:

## Open Questions
- ...

## Checks Confirmed
- ...

## Residual Risk
- ...
```

When there are no findings, say so explicitly and still record the high-risk surfaces checked.

## Validation

Confirm that every blocking finding is actionable and evidenced, unsupported claims remain explicit, and the review does not demand unrelated roadmap work. A review should prevent architectural drift without turning one bounded slice into a phase-wide rewrite.

## References

- `references/invariant-matrix.md`
- `../../../docs/PROJECT_DESIGN_CANONICAL.md`
- `../../../ARCHITECTURE.md`
- `../../../COMPATIBILITY.md`
