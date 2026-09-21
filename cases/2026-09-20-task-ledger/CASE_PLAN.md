# First-user black-box case: task ledger

Status: complete
Date: 2026-09-20  
Test operator: Codex acting as a new `rupi` user  
Model: local llama.cpp, `qwen3.8-flash-next`

## Purpose

This case tests the documented first-user workflow end to end. The operator has
general coding-agent experience but no prior knowledge of `rupi`. The `rupi`
implementation is treated as immutable for this case: observed problems will be
documented, not fixed. The only intended repository changes are this case record
and the toy project used as its test subject.

## Bounded slice

1. Choose and specify a small project with objective acceptance checks.
2. Configure a local model using the public quickstart shape.
3. Use `rupi run` to implement the project in a confined workspace.
4. Continue with at least one resumed turn and inspect `trace`/`replay`.
5. Run the project checks independently and compare the result with the spec.
6. Record UX friction, bugs, confusing output, recovery behavior, and unresolved
   questions in `OBSERVATIONS.md` and `FINAL_REPORT.md`.

## Ownership and non-goals

Owned paths:

- `docs/cases/2026-09-20-task-ledger/`

Explicitly not changed:

- `src/`
- `crates/`
- `tests/`
- canonical project documents and roadmap status

The toy project is deliberately small. This case does not evaluate remote
providers, MCP, Pi package installation, extension hosts, or performance claims
that require a separate benchmark.

## Evidence plan

- Preserve the exact prompts and command shapes in the observation log.
- Capture stdout, stderr, exit status, elapsed time, and relevant filesystem
  state for each `rupi` turn.
- Check the endpoint before and after model-driven work.
- Treat a failure as a product observation even when a retry succeeds.
- Keep generated `.rupi-state/` out of the committed case unless a sanitized
  excerpt is needed to prove a finding.

## Finding vocabulary

- **Bug:** behavior contradicts the manual or a stable safety/contract promise.
- **UX friction:** behavior works, but a new user must guess, repeat, or perform
  avoidable setup.
- **Documentation gap:** the behavior may be intentional, but the manual does
  not make the required action or limitation discoverable.
- **Observation:** notable behavior without enough evidence to classify as a bug.

Severity in the final report will be `blocker`, `high`, `medium`, or `low`, based
on whether it prevents a first successful task, risks data/safety, interrupts a
normal workflow, or merely adds explanation/effort.
