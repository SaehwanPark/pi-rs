# Loop 8 live case: Lease Receipt

Status: design and oracle baseline in progress
Date: 2026-09-20
Test operator: Codex acting as a fresh `rupi` user
Model target: local Qwen-compatible server, `qwen3.8-flash-next`

## T and ladder rationale

Build **Lease Receipt**, a dependency-free Python 3 HTTP/SQLite pipeline service
with a bounded direct-argv worker. It retains Lease Fence's authenticated
admission, atomic graph validation, ordered fan-out/barrier fan-in, declared
output references, retryable/terminal/blocked states, lease reclaim, private
claim fencing, restart persistence, and fresh-process sink protocol.

Lease Receipt adds exactly one bounded reliability dimension: a stable,
non-secret delivery key and a durable sink receipt across a lost acknowledgement.
The independent oracle uses a sink that records the side effect and then loses
the first response. After the worker's lease is reclaimed, a fresh worker sends
the same delivery key; the sink returns its stored receipt and output without
applying the logical side effect twice. The old private claim token remains
hidden and stale finalization remains fenced.

This proves a small at-least-once/idempotent-sink recovery contract. It does not
claim exactly-once delivery for arbitrary external programs. The sink owns
receipt durability; the worker owns stable-key propagation, durable receipt
recording, lease/fence safety, and honest retry state.

## Bounded slice and ownership

The model must build `project/leasereceipt` from `project/SPEC.md`, including a
readable `README.md` and focused project tests. The independent fresh-process
oracle is `acceptance/test_lease_receipt.py` and its sink helper; it must not
import project modules.

Owned paths:

- `docs/cases/2026-09-20-lease-receipt/`

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

The oracle starts the service, workers, and sink processes separately and checks:

- invalid HMAC and malformed/cyclic admission do not mutate state;
- valid admission, exact idempotency, conflict detection, ordered
  fan-out/barrier/publish delivery, declared scalar inputs, and restart state;
- retryable and terminal sink outcomes plus blocked dependents;
- lease expiry/reclaim and private-token stale-worker rejection;
- lost acknowledgement recovery through a fresh worker, one stable delivery
  key, one durable sink receipt, and one logical side effect;
- direct argv, useful help output, and standard-library-only imports.

No model-written summary is acceptance evidence. A model turn counts as
successful authoring only if the model's files pass both gates without tester
repair.

## Invariants and stopgates

The case must preserve these rupi-relevant invariants:

- one active local model; no voting, delegation, or orchestration policy;
- explicit model/reasoning provenance in trace/session records;
- mutating uncertainty remains explicit and is never blindly replayed;
- `trace` and `replay` stay read-only and do not execute historical tools;
- durable job state, not replay, controls sink side effects;
- private claim fencing prevents stale finalization from changing newer state;
- delivery keys are stable but do not expose private claim tokens or secrets;
- receipt replay is explicit idempotent-sink recovery, not an exactly-once claim;
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
  requests, with no writes permitted by the prompt.
- Recovery may continue only the same incomplete workspace. It may not weaken
  the specification, oracle, delivery-key rule, receipt rule, fencing rule, or
  acceptance gates.
- The tester may repair only case-local implementation/tests/README after the
  bounded model attempts, with exact files and reasons recorded.

Every invocation records its exact prompt hash, config hash, command, session ID,
request count, elapsed time, exit status, stdout/stderr outcome, and trace and
replay commands in `OBSERVATIONS.md`.

## Expected evidence disposition

The pre-implementation oracle is expected to fail with missing
`leasereceipt`; that is a baseline, not acceptance. If model authoring stops,
the final report must distinguish model-created files from tester repair and
must not call the project model-complete. If both independent gates pass after
repair, T is accepted as a tester case while any model-authoring stopgate
remains visible.
