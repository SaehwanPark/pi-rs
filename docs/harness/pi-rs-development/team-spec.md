# pi-rs Development Harness

## Domain Summary

`pi-rs` is currently at its repository-and-contract foundation. Its highest-value reusable workflow is not broad autonomous implementation; it is disciplined delivery of small runtime slices while protecting explicit state, provenance, recovery, compatibility, and startup boundaries.

The harness therefore focuses on three repository-specific specialties:

1. core contract design;
2. Pi behavioral compatibility and fixtures;
3. cross-cutting invariant review.

Generic Rust implementation, functional design, code review, and concise coding skills already exist outside the repository and should be reused rather than copied here.

## Task Inventory

- define typed runtime contracts and transitions;
- implement bounded roadmap slices;
- add deterministic unit and failure-path tests;
- build Pi compatibility surfaces with fixtures;
- review persistence, provenance, context, tool, failover, MCP, startup, and trust implications;
- reconcile architecture, compatibility, and roadmap documentation.

## Reuse Notes

- `AGENTS.md` already provides durable repo-wide purpose, boundaries, style, performance, event, session, tool, context, failover, MCP, compatibility, TUI, security, and definition-of-done guidance. It should not be expanded with this playbook.
- `docs/PROJECT_DESIGN_CANONICAL.md` remains the design authority.
- `ARCHITECTURE.md`, `COMPATIBILITY.md`, and `ROADMAP.md` remain operational state.
- General-purpose implementation and review skills should complement, not be duplicated by, this harness.

## Architecture

**Outermost pattern:** Pipeline.

```text
bound slice -> specialist design/fixture work -> implementation -> invariant review -> verification
```

**Local pattern:** Expert Pool. Only specialists triggered by the changed surface are selected.

**Quality gate:** Producer-Reviewer for non-trivial runtime changes, with one bounded revision pass.

This stays single-agent by default. Delegation is optional for independent read-heavy investigation, specialized review, or isolated non-overlapping work.

## Delegation Decision Gate

Before delegation, record:

- independent work unit and expected specialization/context benefit;
- read/write ownership and any stateful test resource;
- required permissions and tools;
- synthesis owner (normally the primary implementer);
- partial-failure and conflict reporting.

Do not parallelize writers in one checkout. Use non-overlapping paths or isolated worktrees. Keep delegation depth to one layer.

## Roles

| Role | Responsibility | Skill | Writes |
| --- | --- | --- | --- |
| Change owner | Bounds, synthesizes, verifies, and accepts the slice; implements unless an assigned specialist owns paths | `.agents/skills/pi-rs-change-orchestrator/SKILL.md` | unassigned source/tests/docs and optional `00`, `90` artifacts |
| Contract designer | Defines typed runtime states and failure semantics; advises read-only or becomes sole writer for assigned paths | `.agents/skills/pi-rs-core-contract-designer/SKILL.md` | assigned contract code/tests and optional `10_contract-notes.md` |
| Compatibility author | Captures Pi-facing behavior in fixtures and diagnostics; advises read-only or becomes sole writer for assigned paths | `.agents/skills/pi-rs-compatibility-fixture-author/SKILL.md` | assigned compatibility code/tests and optional `20_compatibility-report.md` |
| Invariant reviewer | Performs read-only architecture-sensitive review | `.agents/skills/pi-rs-invariant-reviewer/SKILL.md` | optional `80_invariant-review.md` only |

The change owner is always the synthesis and final acceptance owner. A delegated producer's mode and owned paths must be declared before work begins; the change owner does not duplicate those writes.

## Routing Matrix

Producer/design routing and review routing are independent axes.

| Request signal | Producer/design route | Review trigger |
| --- | --- | --- |
| Typo, formatting, isolated prose | direct work; no harness artifacts | none |
| Event, ID, state machine, provider error, session, trace, tool lifecycle, provenance, epoch, context contract, failover contract, redaction, trust | contract designer | invariant reviewer after implementation |
| Pi skill, prompt, package, session import/export, theme, extension API | compatibility author | invariant reviewer when runtime boundaries or claims change |
| Startup, persistence, tool mutation, security, architecture boundary without a new contract | change owner | invariant reviewer after implementation |
| Mixed runtime + compatibility change | contract designer and compatibility author with disjoint ownership | invariant reviewer after implementation |

## Artifact Convention

Durable handoffs are optional for routine single-session work and required when work is delegated, resumable, disputed, or audit-sensitive. `_workspace/` is local working state, ignored by Git, and retained only as long as the active change needs it. Move lasting decisions into source, tests, or canonical documentation before cleanup.

```text
_workspace/changes/{change-id}/
├── 00_change-brief.md
├── 10_contract-notes.md
├── 20_compatibility-report.md
├── 80_invariant-review.md
└── 90_validation-report.md
```

`{change-id}` is stable lower-kebab case derived from the roadmap item or issue, such as `phase-0-agent-event`.

Each artifact begins with:

```markdown
# Title

Change: {change-id}
Owner: {role}
Status: draft | ready | blocked | superseded
Inputs: paths or artifact names
```

Do not create placeholder artifacts for specialists that were not selected.

## Phase Order and Handoffs

### Phase 1: Bound

- Inputs: user request, repository state, canonical docs.
- Actions: classify boundary, roadmap item, non-goals, owned paths, tests.
- Output: in-thread brief or `00_change-brief.md`.
- Complete when one vertical slice and acceptance evidence are explicit.

### Phase 2: Specialize

- Inputs: change brief and declared specialist mode/owned paths.
- Actions: define core contract and/or compatibility behavior only when routed; a specialist writes code/tests only when assigned as sole writer for those paths.
- Outputs: decision-complete advice or assigned code/tests, plus optional `10_contract-notes.md` or `20_compatibility-report.md`.
- Complete when states, failures, fixture behavior, exclusions, and write ownership are explicit.

### Phase 3: Implement

- Inputs: brief and selected specialist output.
- Actions: the change owner implements unassigned paths and integrates specialist-owned results without duplicating writes.
- Output: source, tests, and necessary docs.
- Complete when behavior and failure paths are represented in code and tests under one writer per path.

### Phase 4: Review

- Inputs: original request, brief, diff, and available test evidence; final verification may still be pending.
- Actions: targeted invariant review.
- Output: verdict in-thread or `80_invariant-review.md`.
- Complete on `pass`, or after one focused `fix` revision and recheck.

### Phase 5: Verify

- Inputs: final diff and review.
- Actions: formatting, linting, tests, fixture checks, and applicable performance/security checks.
- Output: in-thread evidence or `90_validation-report.md`.
- Complete when exact commands, outcomes, residual risks, and roadmap/doc reconciliation are recorded.

## Failure Policy

- **Missing public-behavior decision:** ask the user; do not guess a compatibility, storage, or security contract.
- **Missing upstream Pi evidence:** keep unevaluated in-scope public behavior `Unknown`; keep undocumented internals and arbitrary internal imports `Unsupported`; use `Experimental` only for an implemented, fixture-backed subset.
- **Specialist failure:** continue only if its surface is not required; otherwise stop with its incomplete output and uncertainty preserved.
- **Conflicting specialist conclusions:** the change owner compares both against canonical invariants and records the unresolved decision; no majority vote.
- **Review `fix`:** allow one focused revision.
- **Review `redo` or repeated blocking finding:** stop and escalate the architecture decision.
- **Failed verification:** do not mark roadmap work complete. Report command output and whether the failure is caused by the change.
- **Uncertain mutating operation:** preserve `Unknown`; never replay automatically.

## Removable Runtime-Specific Logic

No model pin, MCP dependency, or mandatory subagent runtime is part of the harness. Optional delegation instructions can be removed without changing the artifact or quality contracts.

## Validation Checklist

- all skill files have `name` and `description` YAML frontmatter;
- every linked repository path exists;
- routing boundaries do not overlap silently;
- direct work remains the default for small changes;
- the change owner remains the single synthesis owner;
- parallel writes require non-overlapping ownership or isolation;
- failure policy preserves unknowns and partial results;
- specialist outputs and artifact names agree across skills and this spec.

## Scenario Tests

### Normal: Phase 0 event contract

Request: “Define the initial `AgentEvent` identity and ordering contract with tests.”

Expected route: change orchestrator -> core contract designer -> implementation -> invariant reviewer -> verification.

Expected evidence: typed IDs/events, ordering tests, persistence/replay/UI notes, roadmap update only when status changes.

### Normal: Pi skill discovery compatibility

Request: “Add project-local `SKILL.md` discovery compatible with Pi.”

Expected route: change orchestrator -> compatibility author -> implementation -> invariant reviewer -> verification.

Expected evidence: positive and malformed fixtures, relative resource handling, explicit compatibility status, no Node startup for data-only skills.

### Near miss: direct documentation edit

Request: “Fix a typo in the README.”

Expected route: direct work. No `_workspace/` artifacts and no specialist delegation.

### Failure: undocumented extension behavior

Request: “Claim full support for an undocumented Pi extension import observed in one package.”

Expected behavior: refuse the broad claim and keep the undocumented internal import `Unsupported`. Request an in-scope versioned/documented public target before adding support; use `Experimental` only after a bounded implementation and fixture exist.

### Failure: overlapping parallel writes

Request: “Have two workers edit the same event enum and merge the best result.”

Expected behavior: reject shared-checkout parallel writes; serialize under one owner or isolate alternatives, then let one owner synthesize and verify.
