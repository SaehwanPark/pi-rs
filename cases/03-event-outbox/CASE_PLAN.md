# Loop 2 live case: Event Outbox

Status: complete; acceptance passed
Date: 2026-09-20
Test operator: Codex acting as a fresh `rupi` user
Model target: local `qwen3.8-flash-next`

## T and why it is next

Build **Event Outbox**, a dependency-free Python 3 service that accepts durable
events over HTTP and delivers them through a separate worker process using a
small newline-delimited JSON protocol.

The Test Ledger case covered a local CLI, JSON-file persistence, validation, and
fresh Python processes. Read Queue added a long-lived HTTP server, SQLite,
restart recovery, and an independent HTTP oracle. Event Outbox is incrementally
harder because it keeps those boundaries and adds an outbox state machine,
idempotent event admission, retryable delivery state, and a separate process/
argv/protocol integration boundary. It remains bounded: one service, one worker
mode, one sink protocol, no authentication or deployment system.

## Bounded specification

Owned paths are `docs/cases/2026-09-20-event-outbox/` only. The model's project
workspace is `project/`; the independent oracle is `acceptance/` and does not
import the project implementation.

The model must build the project described by `project/SPEC.md`, including a
readable `README.md`, implementation modules, and project tests. The project
must use only the Python standard library.

## Independent acceptance oracle

`acceptance/test_event_outbox.py` starts the HTTP service in a fresh process,
uses HTTP requests from a separate Python process, runs the worker as another
fresh process, and supplies an independent fake sink process through direct
argv. It verifies:

1. health and empty event lookup;
2. event admission, deterministic status, same-id idempotency, and conflicting
   duplicate rejection;
3. worker-to-sink NDJSON delivery and durable `delivered` state;
4. a sink rejection leaving an event retryable, with attempts and error
   persisted, followed by a successful retry;
5. malformed/invalid HTTP requests returning errors without changing accepted
   events;
6. stopping and restarting the server against the same SQLite file preserves
   delivery state;
7. `--help`, `serve --help`, and `worker --help` succeed.

The oracle must pass independently of any final prose produced by the model.
Before implementation it is expected to fail because `project/outbox` does not
exist; that baseline failure is recorded before the first live run.

## Live procedure and evidence

- Create and push the temporary branch from clean `origin/main`.
- Commit this plan, `SPEC.md`, the oracle, fake sink, config, and ignore rules
  before implementation, then open a draft PR immediately.
- Use the actual `rupi run` CLI from `project/` with bounded implementation and
  verification prompts. Prefer the direct `process` tool for known programs on
  Windows and do not ask the model to treat its own summary as proof.
- Record every live command, prompt, session id, model-request count, elapsed
  time, stdout/stderr status, trace/replay result, project-suite result, oracle
  result, and material friction in `OBSERVATIONS.md`.
- If manual project-local repair is needed after a live turn, keep it bounded,
  list exact changes and reason, and continue to treat the model turn as
  incomplete evidence.
- Do not modify rupi runtime/source/tests for this case. If a runtime defect is
  necessary to make progress, stop and record the smallest reproducible issue.

## Non-goals and stopgates

Non-goals are authentication, HTTPS, concurrent-write guarantees, migrations,
third-party packages, a background daemon, a UI, arbitrary sink protocols, and
changes to canonical rupi documents or roadmap status.

Stop only for a concrete environment/runtime gate after bounded recovery
attempts: unavailable endpoint, inability to create/push the authorized branch
or PR, a repeated provider failure, or a project requirement that conflicts with
the spec. A model request-budget exhaustion is evidence to record, not proof of
acceptance and not by itself a reason to abandon the bounded repair/verification
path.

## Completion note

The endpoint was available. Three bounded implementation attempts did not write
the project before being interrupted or reaching an output-length boundary, so
the tester completed the small project-local implementation without changing
rupi. A final read-only `rupi run` verified the project suite and help commands;
the independent fresh-process oracle and final project suite also passed.
