# Loop 9 fresh-memory case: Receipt Ledger

Status: design and oracle baseline
Date: 2026-09-20
Test operator: Codex acting as a fresh `rupi` user
Model target: local `qwen3.8-flash-next` at `http://127.0.0.1:8000`
Branch: `tester/2026-09-20-loop-9-receipt-ledger`
Base: `main` at the merged Lease Receipt baseline

## T and ladder rationale

Build **Receipt Ledger**, a dependency-free Python 3 HTTP/SQLite pipeline
worker. It inherits Lease Receipt's authenticated atomic admission, ordered
dependency and barrier fan-in data flow, bounded direct-argv sinks, retryable
and terminal states, lease reclaim, private claim-token fencing, stable
delivery keys, durable sink receipts, and explicit at-least-once semantics.

Receipt Ledger adds exactly one capstone dimension: an append-only,
tamper-evident operational audit ledger. Every authoritative admission or job
state transition is committed with the state mutation in SQLite as a
canonical JSON event chained by SHA-256. A read-only `audit --verify` command
recomputes sequence continuity and the hash chain in a fresh process. Audit
records contain safe public identifiers and observed outcomes only; they never
contain the HMAC secret or private lease tokens. The audit ledger is evidence,
not a work queue and never controls side effects.

The new dimension is deliberately narrow. It is not a new scheduler feature,
query language, migration system, concurrent-worker guarantee, or exactly-once
claim. A lost sink acknowledgement remains an unproven/unknown side effect;
the worker records no success until a matching receipt is observed, and a
fresh worker reuses the same delivery key.

## Bounded slice and ownership

The model must build `project/receiptledger` from `project/SPEC.md`, including
a readable `README.md` and focused project tests. The independent fresh-process
oracle lives in `acceptance/` and imports no project implementation module.

Owned paths:

- `docs/cases/2026-09-20-receipt-ledger/`

Explicitly not changed:

- `src/`, `crates/`, repository `tests/`, and runtime source;
- `AGENTS.md`, canonical design, architecture, compatibility, roadmap,
  repository README, user manual, and all parent integration documents.

After the bounded model attempts, a tester repair may modify only files under
this case directory. Every repair is listed with exact paths and reasons and
is never called model completion. Oracle-only repairs are allowed only when an
independent run proves an oracle defect.

## Acceptance gates

Both gates must pass independently:

1. from `project/`, the project unittest suite passes with warnings treated as
   errors;
2. from the case root, the fresh-process oracle passes.

The oracle starts service, worker, sink, audit, and tamper-check processes
separately. It checks inherited Lease Receipt behavior plus the new ledger:

- invalid HMAC and malformed/cyclic admission do not mutate durable state;
- idempotent/conflicting admission is deterministic;
- source/fan-out/barrier/publish delivery preserves declared data flow;
- retryable and terminal outcomes, blocked dependents, lease reclaim, and
  stale-worker non-overwrite remain correct;
- a lost acknowledgement is not turned into success and a fresh worker
  recovers one sink receipt with the same stable delivery key;
- audit rows are committed with state transitions, sequence numbers are
  contiguous, the SHA-256 chain verifies after restart, and the audit never
  leaks secrets or private claim tokens;
- direct argv, useful help, standard-library-only imports, and a tampered
  audit row detected by an independent fresh-process verifier.

No model-written summary is acceptance evidence. A model-authoring attempt is
successful only if its own project suite and independent oracle pass without
tester repair.

## Audit contract under test

The project spec is authoritative, but the case boundary is fixed here:

- the SQLite job/pipeline state remains the side-effect authority;
- audit rows are inserted in the same transaction as the state mutation;
- each row stores a contiguous `seq`, canonical public `event_json`,
  `prev_hash`, and `event_hash`;
- `event_hash = SHA256(prev_hash + "\\n" + canonical_event_json)`;
- the first `prev_hash` is 64 zeroes;
- event JSON is deterministic and contains no secret, private claim token, or
  raw sink command arguments;
- the verifier opens SQLite read-only and never repairs, truncates, or replays
  the ledger;
- missing, reordered, edited, or hash-inconsistent rows fail verification;
- audit output is a projection and cannot be used to re-run a sink.

The audit event vocabulary is bounded to admission and operational job
transitions: `pipeline_admitted`, `job_claimed`, `lease_reclaimed`,
`job_succeeded`, `job_retryable_failure`, `job_failed`, `job_blocked`, and
`stale_finalization_rejected`. An unknown sink completion is represented by a
retryable failure with `outcome: "unknown"` only when the worker has enough
control to persist that observation; a killed worker leaves only its committed
claim and the later reclaim evidence.

## Bounded model/recovery policy

- Initial authoring: `project/rupi.config.json`, at most **6 model requests**,
  **90,000 ms per provider request**, `thinking: low`.
- Recovery authoring: `project/rupi.recovery.config.json`, at most **4 model
  requests**, **90,000 ms per provider request**, `thinking: low`; it continues
  the same incomplete workspace and may not weaken the spec or oracle.
- Read-only verification, only if justified after authoring, uses
  `project/rupi.verify.config.json`, at most **3 model requests** and the same
  provider timeout, with no writes permitted by the prompt.

The request limits and timeouts are fixed before the live runs. No extra model
attempt is made after a budget or provider stopgate. A slow model, output-limit
boundary, request budget, or incomplete turn is evidence, not acceptance and
does not by itself block bounded tester repair.

## Evidence discipline

Record in `OBSERVATIONS.md`:

- exact endpoint check and binary path;
- pre-implementation oracle command, exit status, elapsed time, and failure;
- exact model commands, prompt/config SHA-256 hashes, session IDs, request
  counts, per-request/provider timing, exit statuses, and stdout/stderr;
- files written by each model attempt and tool outcomes;
- independent project-suite and oracle commands/results;
- trace/replay commands and proof they were read-only;
- repair boundary, exact changed files, and reasons;
- invariant review, repository checks, and startup benchmark.

The oracle and project tests use fresh Python processes for service, worker,
and sink boundaries. Temporary state, `.rupi-state*`, bytecode, and logs are
ignored unless a sanitized excerpt is necessary to prove a finding.

## Stopgates and invariants

Concrete stopgates are: unavailable local endpoint after verification,
inability to create/push this branch or the authorized draft PR, repeated
provider failure, or a project requirement that cannot be satisfied without
changing rupi source/tests. Preserve the exact reproduction and stop only
after the bounded recovery policy in that case. Do not weaken the audit chain,
receipt, fencing, `Unknown`, single-model, lazy-startup, direct-argv, or
at-least-once contracts to clear a gate.

Before final handoff, apply the repository invariant review to the case diff:

- trace/replay stay read-only and never execute historical tools;
- one local model and native reasoning provenance remain explicit;
- uncertain mutation is never blindly replayed;
- the audit ledger is not confused with rupi's canonical trace or model context;
- private claim identity is never exposed as a delivery identity;
- optional integrations and backup models remain lazy;
- no case artifact claims exactly-once delivery for arbitrary sinks.
