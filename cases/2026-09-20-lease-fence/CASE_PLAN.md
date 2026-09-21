# Loop 7 live case: Lease Fence

Status: complete as tester case; model-authoring stopgate open
Date: 2026-09-20
Test operator: Codex acting as a fresh `rupi` user
Model target: local Qwen-compatible server, `qwen3.8-flash-next`

## T and ladder rationale

Build **Lease Fence**, a dependency-free Python 3 HTTP/SQLite pipeline service
with a bounded direct-argv worker. It retains Lease Cascade's signed atomic
admission, ordered dependency/barrier fan-in, declared output references,
retryable/terminal/blocked state, leases, reclaim, restart persistence, and
fresh-process sink protocol.

Lease Fence is incrementally harder in exactly one reliability dimension:
Lease Cascade proves that an expired lease can be reclaimed, but it does not
prove that the original worker's late sink response is harmless. Lease Fence
stores a private per-claim fencing token and finalizes a result only when the
token still owns the lease. The independent oracle deliberately runs two fresh
workers: worker A blocks after claiming, worker B reclaims after expiry and
succeeds, then worker A returns. The durable winner must remain B's result and
the stale worker must report a non-zero stale-lease outcome without changing
the row.

This is a small distributed-state reliability fixture, not a general workflow
engine or scheduler. It increases difficulty through stale completion fencing,
not through another broad feature collection.

## Bounded slice and ownership

The model must build `project/leasefence` from `project/SPEC.md`, including a
readable `README.md` and focused project tests. The independent fresh-process
oracle is `acceptance/test_lease_fence.py`; it must not import project modules.

Owned paths:

- `docs/cases/2026-09-20-lease-fence/`

Explicitly not changed:

- `src/`, `crates/`, repository `tests/`, and runtime source;
- `AGENTS.md`, canonical design, architecture, compatibility, roadmap, README,
  user manual, and all parent integration documents.

The tester may make a clearly listed, bounded repair only inside this case
directory when a model turn is incomplete. A tester repair is never counted as
model-authoring evidence. The tester may repair the independent oracle only for
an oracle defect discovered by an independent run, and must list that repair.

## Acceptance gates

Both gates must pass independently:

1. from `project/`, the project unittest suite passes with
   `-W error::ResourceWarning`;
2. from the case root, the fresh-process oracle passes.

The oracle starts the service, workers, and sink processes separately and
checks:

- invalid HMAC and malformed/cyclic admission do not mutate state;
- valid admission, exact idempotency, conflict detection, and restart state;
- ordered source/fan-out/barrier/publish delivery;
- declared scalar inputs and ordered barrier fan-in only;
- retryable and terminal sink outcomes plus blocked dependents;
- lease expiry and reclaim;
- stale worker completion cannot overwrite a newer successful claim;
- direct argv, help output, and standard-library-only imports.

No model-written summary is acceptance evidence. A model turn counts as
successful authoring only if the model's files pass both gates without tester
repair.

## Invariants and stopgates

The case must preserve these rupi-relevant invariants:

- one active local model; no voting, delegation, or orchestration policy;
- explicit model/reasoning provenance in the trace/session records;
- mutating uncertainty remains explicit and is never blindly replayed;
- `trace` and `replay` stay read-only and do not execute historical tools;
- durable job state, not replay, controls sink side effects;
- a stale lease response is never silently counted as a current success;
- optional systems and backup models remain lazy because the case does not
  configure them.

Concrete stopgates are: unavailable provider endpoint after verification,
inability to create/push the authorized branch or draft PR, repeated provider
failure, or a project requirement that conflicts with an rupi contract and
cannot be worked around inside this case. Slow local generation, a request
budget, and an incomplete model turn are evidence and trigger the bounded
repair/recovery policy; they are not acceptance.

## Bounded model/recovery policy

- Initial authoring: `rupi.config.json`, at most 6 model requests, 90,000 ms
  per provider request, `thinking: low`.
- Recovery authoring: `rupi.recovery.config.json`, at most 4 model requests,
  90,000 ms per provider request, `thinking: low`.
- Read-only verification, if used: `rupi.verify.config.json`, at most 3 model
  requests, and no writes permitted by the prompt.
- A recovery attempt may continue only the same incomplete workspace. It may
  not weaken the specification, oracle, lease fencing rule, or acceptance
  gates.
- The tester may repair only case-local implementation/tests/README after the
  bounded model attempts, with exact files and reasons recorded.

Every invocation records its exact prompt hash, config, command, session ID,
request count, elapsed time, exit status, stdout/stderr outcome, and trace and
replay commands in `OBSERVATIONS.md`.

## Expected evidence disposition

The pre-implementation oracle is expected to fail with missing
`leasefence`; that is a baseline, not acceptance. If model authoring stops,
the final report must distinguish model-created files from tester repair and
must not call the project model-complete. If both independent gates pass after
repair, T is accepted as a tester case while any model-authoring stopgate
remains visible.
