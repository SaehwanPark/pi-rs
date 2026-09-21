# Loop 6 live case: Lease Cascade

Status: draft — implementation and live evidence pending  
Date: 2026-09-20  
Test operator: Codex acting as a fresh `rupi` user  
Model target: local Qwen-compatible server, `qwen3.8-flash-next`  
Branch: `tester/2026-09-20-loop-6-lease-cascade`  
Base: `main` at `e450c6c`

## T and why it is next

Build **Lease Cascade**, a dependency-free Python 3 HTTP/SQLite service that
accepts signed pipelines and delivers runnable jobs through a separate bounded
worker process.

The checked-in ladder is:

1. Test Ledger: local CLI, JSON-file persistence, validation, and fresh
   processes.
2. Read Queue: long-lived HTTP, SQLite persistence, CRUD validation, and server
   restart recovery.
3. Event Outbox: idempotent HTTP admission, persisted retry state, and a
   direct-argv NDJSON sink worker.
4. Webhook Inbox: HMAC admission, delivery leases, crash reclaim, and
   restart-safe at-least-once delivery.
5. Batch Relay: atomic dependency-DAG admission, retryable/terminal failures,
   blocked dependents, lease reclaim, and a direct-argv sink.
6. Artifact Pipeline: declared top-level output-to-input references across a
   dependency DAG.

Lease Cascade keeps Artifact Pipeline's signed atomic admission, durable job
state, leases, restart behavior, direct-argv sink, and declared scalar data
flow. It adds one bounded fan-out/fan-in contract: a `barrier` job explicitly
selects one top-level output field from each successful direct dependency and
receives an ordered collection. A missing selected field fails the barrier
locally and blocks its downstream cascade. This is a narrow increase in
state/data-flow complexity, not a general workflow engine.

## Bounded slice and ownership

The model must build `project/leasecascade` from `project/SPEC.md`, including a
readable `README.md` and focused project tests. The independent fresh-process
oracle lives in `acceptance/` and must not import project implementation
modules.

Owned paths:

- `docs/cases/2026-09-20-lease-cascade/`

Explicitly not changed:

- `src/`, `crates/`, repository `tests/`, and repository runtime code;
- `AGENTS.md`, canonical design, architecture, compatibility, roadmap, and
  other parent integration documents.

The tester may make a bounded project-local repair if a live model turn is
incomplete. Every repair must be listed separately and is not model-authoring
evidence. The tester may repair the independent oracle only to correct an oracle
defect discovered by its own independent run; the repair must be listed.

## Acceptance and evidence discipline

Acceptance requires both independent gates to pass:

1. the project unittest suite from `project/`;
2. the fresh-process oracle from `acceptance/`.

The oracle starts the service, worker, and sink as fresh processes. It checks
authentication, atomic admission, idempotency/conflicts, fan-out order,
fan-in selection and ordering, output-to-input propagation, retryable and
terminal failures, blocked barriers, lease reclaim, restart persistence, and
the standard-library-only boundary. It imports no project implementation
module.

No model-written summary is acceptance evidence. A model turn counts as
successful authoring only if its own project files pass both gates without
tester repair. Tester-authored implementation, tests, or oracle changes remain
explicitly separate.

Trace and replay are read-only checks. They must not execute recorded tools,
contact the provider, or be used to claim that the model completed the project.

## Invariant checks

The live run must preserve the rupi contracts relevant to this case:

- one active model; no model voting, delegation, or orchestration policy;
- model and reasoning provenance remain explicit in trace/session evidence;
- mutating tool uncertainty remains `Unknown` and is never blindly replayed;
- optional systems and the backup remain lazy on the startup path;
- the worker's durable state, not historical replay, controls side effects;
- failed or unknown sink work is not silently counted as success.

The case does not require changing any rupi contract. A concrete runtime issue
that cannot be worked around inside the case is preserved as a reproduction and
reported as a stopgate; core semantics are not weakened to clear acceptance.

## Bounded procedure and stopgates

1. Run the independent oracle before implementation and record its expected
   missing-module failure.
2. Confirm the local Qwen endpoint and use the actual `rupi run` binary with the
   committed initial config and prompt.
3. Record exact commands, prompts, session IDs, request counts, elapsed times,
   stdout/stderr status, tool outcomes, friction, repairs, and stopgates in
   `OBSERVATIONS.md`.
4. Run the project suite independently, then the independent oracle
   independently, then `rupi trace` and `rupi replay --tools --sequence` for
   every material session.
5. Retry only within the committed initial/recovery budgets. Do not silently
   widen deadlines or request counts. If a concrete rupi issue blocks progress,
   preserve the reproduction and stop rather than changing rupi source/tests.
6. If tester repair is needed, record the exact files and reason, rerun both
   gates, and keep the result labelled tester-authored.
7. Run the repository checks proportionate to this docs/case-only slice and
   report unrelated pre-existing failures separately.

Stopgates are: unavailable endpoint, inability to create/push the authorized
branch or draft PR, repeated provider failure, or a project requirement that
conflicts with a rupi contract and cannot be worked around without changing
rupi source/tests. Slow generation, a request budget, and an incomplete model
turn are evidence, not acceptance.

## Non-goals

- third-party Python packages, deployment packaging, HTTPS, multiple users,
  key rotation, or authorization beyond one shared HMAC secret;
- concurrent-worker guarantees, migrations, graph editing, polling daemons,
  scheduled retries, cancellation APIs, or arbitrary nested expressions;
- arbitrary sink protocols, shell pipelines, network calls from the worker, or
  exactly-once delivery;
- collection of more than one selected top-level field per barrier;
- modifying rupi runtime/source/tests or parent integration documents.
