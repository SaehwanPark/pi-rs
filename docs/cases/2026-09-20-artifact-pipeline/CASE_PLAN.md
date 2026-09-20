# Loop 5 live case: Artifact Pipeline

Status: in progress
Date: 2026-09-20
Test operator: Codex acting as a fresh `rupi` user
Model target: local llama.cpp, `qwen3.8-flash-next`
Branch: `tester/2026-09-20-loop-5-artifact-pipeline`
Base: `origin/main` at `959fe7e`

## T and why it is next

Build **Artifact Pipeline**, a dependency-free Python 3 HTTP/SQLite service that
accepts signed pipelines and delivers runnable jobs through a separate bounded
worker process.

The checked-in ladder is:

1. Test Ledger: local CLI, JSON-file persistence, validation, and fresh
   processes.
2. Read Queue: long-lived HTTP, SQLite persistence, CRUD validation, and server
   restart recovery.
3. Event Outbox: idempotent HTTP admission, persisted retry state, and a
   direct-argv NDJSON sink worker.
4. Webhook Inbox: HMAC admission, delivery leases, crash reclaim, and restart-
   safe at-least-once delivery.
5. Batch Relay: all of the above plus atomic dependency-DAG admission,
   retryable/terminal failures, and blocked dependents.

Artifact Pipeline retains Batch Relay's reliability and process boundaries and
adds one focused data-flow contract: a successful job stores a JSON `output`,
and a downstream job receives only explicitly declared top-level fields from
successful dependency outputs. It is a bounded pipeline/data-flow exercise,
not a general workflow engine.

## Bounded slice and ownership

The model must build `project/artifactpipe` from `project/SPEC.md`, including a
readable `README.md` and focused project tests. The independent fresh-process
oracle lives in `acceptance/` and must not import project implementation
modules.

Owned paths:

- `docs/cases/2026-09-20-artifact-pipeline/`

Explicitly not changed:

- `src/`, `crates/`, repository `tests/`, and repository runtime code;
- `AGENTS.md`, canonical design, architecture, compatibility, roadmap, and
  other parent integration documents.

The tester may make a bounded project-local repair if a live model turn is
incomplete. Every repair must be listed separately and does not become
model-authoring evidence.

## Acceptance and evidence discipline

Acceptance requires both independent gates to pass:

1. the project unittest suite from `project/`;
2. the fresh-process oracle from `acceptance/`.

The oracle independently starts the server, worker, and sink processes, checks
HMAC admission, atomic DAG/reference validation, idempotency/conflict handling,
dependency-ordered delivery, input/output propagation, retryable and terminal
failures, blocked dependents, lease reclaim, and restart persistence. It imports
only Python standard-library modules and does not use project implementation
imports.

No model-written summary is acceptance evidence. A model turn counts as
successful authoring only if its own files pass the project suite and the
independent oracle without tester repair. Trace and replay are read-only checks;
they must not execute recorded tools or contact the provider.

## Bounded procedure and stopgates

1. Run the oracle before implementation and record its expected missing-module
   failure.
2. Confirm the local Qwen endpoint and use the actual `rupi run` binary with the
   committed initial config and prompt.
3. Record exact commands, prompts, session ids, request counts, elapsed times,
   stdout/stderr, tool outcomes, friction, and stopgates in `OBSERVATIONS.md`.
4. Run the project suite independently, then the oracle independently, then
   `rupi trace` and `rupi replay --tools --sequence` for every material session.
5. Retry only with the committed recovery budget. If a concrete rupi issue
   blocks progress, preserve the reproduction and stop rather than weakening
   core semantics or modifying rupi source.
6. If tester repair is needed, record the exact files and reason, re-run both
   gates, and keep the result labelled tester-authored.

The case stopgates are: unavailable endpoint, inability to create/push the
authorized branch or draft PR, repeated provider failure, or a project
requirement that conflicts with the rupi contract and cannot be worked around
without changing rupi source/tests. Slow generation, a request budget, or an
incomplete model turn is evidence, not acceptance.

## Non-goals

- third-party Python packages, deployment packaging, HTTPS, multiple users, or
  key rotation;
- concurrent-worker guarantees, scheduled polling, migrations, graph editing,
  cancellation APIs, or arbitrary nested expression languages;
- exactly-once delivery semantics;
- shell pipelines or network calls from the worker;
- any change to rupi runtime/source/tests or parent project documents.

