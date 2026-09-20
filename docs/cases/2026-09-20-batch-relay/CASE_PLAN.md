# Loop 4 live case: Batch Relay

Status: complete; acceptance passed after bounded tester repair
Date: 2026-09-20
Test operator: Codex acting as a fresh `rupi` user
Model target: local `qwen3.8-flash-next`

## T and why it is next

Build **Batch Relay**, a dependency-free Python 3 HTTP/SQLite service that
accepts signed batches of jobs and delivers runnable jobs through a separate
bounded worker process.

The earlier ladder is:

1. Test Ledger: local CLI, JSON-file persistence, validation, and fresh
   processes.
2. Read Queue: long-lived HTTP, SQLite persistence, CRUD validation, and
   server restart recovery.
3. Event Outbox: idempotent HTTP admission, retry state, and a direct-argv
   NDJSON sink worker.
4. Webhook Inbox: HMAC authentication, delivery leases, crash reclaim, and
   restart-safe at-least-once delivery.

Batch Relay retains the Webhook Inbox boundaries and adds one bounded
stateful layer: an atomically admitted dependency DAG. The worker may deliver
only jobs whose dependencies succeeded; retryable sink failures remain
pending, terminal failures block dependents, and batch status is derived from
the durable job states. This is deliberately a small scheduler exercise, not
a general workflow engine.

## Bounded specification

The model must build `project/batchrelay` from `project/SPEC.md`, including a
readable `README.md` and focused project tests. The independent oracle lives
in `acceptance/`, starts every service, worker, and sink as a fresh process,
and must not import project implementation modules.

Owned paths:

- `docs/cases/2026-09-20-batch-relay/`

Explicitly not changed:

- `src/`
- `crates/`
- `tests/`
- `AGENTS.md`, canonical design, architecture, compatibility, roadmap, and
  other project-state documents

The tester may make a bounded project-local repair if a live model turn is
incomplete. Every such edit must be listed as tester repair evidence and does
not turn the model turn into successful implementation evidence.

## Acceptance surface

The project succeeds only when all of the following are independently true:

- the project unittest suite passes with warnings treated as errors;
- the fresh-process oracle passes signature rejection, atomic DAG validation,
  idempotent/conflicting batch admission, dependency ordering, retryable and
  terminal sink failures, blocked dependents, lease expiry/reclaim, direct-argv
  sink behavior, and server restart recovery;
- top-level, `serve`, and `worker` help commands succeed;
- the project imports only Python standard-library modules;
- the README documents exact commands, request signing, batch/job state
  transitions, dependency rules, lease/reclaim behavior, sink protocol,
  persistence, and checks.

No model-written summary is acceptance evidence.

## Live procedure and evidence

1. Run the independent oracle before any project implementation and record its
   expected missing-module failure.
2. Use the actual `rupi run` CLI from `project/` with the committed config and
   a bounded implementation prompt. On Windows, use the direct `process` tool
   with program/argv for Python and avoid Unix commands and shell quoting.
3. Record every live command, prompt, session id, model-request count, elapsed
   time, stdout/stderr status, and material friction in `OBSERVATIONS.md`.
4. Run the project suite independently, then the independent oracle, then
   `rupi trace` and `rupi replay --tools --sequence` for each material session.
5. Retry only within the bounded case. If a concrete rupi behavior blocks
   progress after bounded recovery attempts, preserve the exact reproduction,
   stop project work, and report it to the parent.

## Finding vocabulary and stopgates

- **Bug:** behavior contradicts the documented rupi contract or a stable
  safety promise.
- **UX friction:** the workflow works but makes a fresh user guess, repeat, or
  perform avoidable setup.
- **Project defect:** an implementation or oracle problem in this case, not a
  rupi finding.
- **Observation:** notable behavior without enough evidence for a stronger
  classification.

Use `blocker`, `high`, `medium`, or `low` severity. A request-budget boundary,
slow local generation, or incomplete model turn is evidence, not acceptance.
Stop only for a concrete environment/runtime gate such as an unavailable
endpoint, inability to create/push the authorized branch or draft PR, repeated
provider failure, or a rupi contract needed by the specification that cannot
be worked around without changing rupi source/tests. Never change rupi to
clear a case stopgate.

## Planned configs

- `project/rupi.config.json`: initial implementation, bounded at eight model
  requests, with the opt-in one-shot progress boundary enabled.
- `project/rupi.recovery.config.json`: focused recovery, six requests and a
  separate state directory.
- `project/rupi.verify.config.json`: read-only project verification with a
  separate state directory.
