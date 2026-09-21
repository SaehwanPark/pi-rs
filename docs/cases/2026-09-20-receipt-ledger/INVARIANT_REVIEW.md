# Invariant review

Change: `2026-09-20-receipt-ledger`  
Owner: fresh-memory tester  
Status: ready  
Verdict: **pass**

Inputs reviewed: `CASE_PLAN.md`, `project/SPEC.md`, the case implementation
and tests, the independent fresh-process oracle, authoring trace/replay
results, repository checks, and `startup-loop9.json`.

## Findings

No release-blocking invariant findings were found. The case is isolated to
`docs/cases/2026-09-20-receipt-ledger/`; it does not change rupi runtime code,
repository tests, canonical design documents, or roadmap state.

| Invariant | Evidence | Result |
| --- | --- | --- |
| Explicit state and uncertain side effects | A lost sink acknowledgement is recorded as unknown/retryable; a worker never invents success without a matching receipt. The oracle verifies replay with the same stable delivery key. | Pass |
| Lease and fencing correctness | Private per-claim tokens gate conditional finalization; expiry/reclaim creates a new attempt; stale finalization cannot overwrite the winner. The oracle checks the stale worker after reclaim. | Pass |
| Public/private identity separation | Public `pipeline_id:job_id` delivery keys are distinct from private claim tokens. HTTP responses, sink argv, receipts, and audit events omit private tokens. | Pass |
| At-least-once boundary | Receipt replay supports an idempotent sink after a lost acknowledgement. The README and spec explicitly do not claim arbitrary exactly-once delivery. | Pass |
| Declared data flow | Dependency inputs and barrier fan-in are validated and ordered; only declared output fields cross job boundaries. The oracle checks five delivery keys and barrier ordering. | Pass |
| State authority versus evidence | SQLite job state remains authoritative for work. The audit table is an append-only evidence projection and `audit --tail` cannot enqueue or replay work. | Pass |
| Atomic audit provenance | Admission and each authoritative job transition append a canonical public event in the same SQLite transaction as the mutation. | Pass |
| Audit integrity | Sequence continuity, canonical JSON, previous hashes, and SHA-256 event hashes are checked by both the project tests and an independent fresh-process recomputation. A tampered row is rejected without repair. | Pass |
| Secret and command safety | HMAC uses the exact request bytes; invalid signatures do not mutate state. Sink commands use direct argv, never a shell; audit data excludes secrets, private tokens, raw argv, and arbitrary sink bytes. | Pass |
| Single-model provenance | Both attempts selected only local `qwen3.8-flash-next`; traces record native reasoning; no backup model or orchestration path was enabled. | Pass |
| Trace/replay safety | Trace and replay were invoked only as read-only inspection. They exited successfully without executing historical tools or mutating the case. | Pass |
| Lazy startup boundary | No rupi source or startup path changed. The startup benchmark remained at 9.736 ms cold and 9.255 ms warm mean. | Pass |

## Residual, intentional boundaries

- An arbitrary external sink can still duplicate an effect; the receipt
  protocol reduces that risk only for a sink honoring the stable delivery key.
- SQLite state and audit rows are protected by the application transaction and
  verified by the hash chain, not by a database trigger or an external
  append-only storage service. Direct database tampering is detected, not
  prevented.
- The case exercises bounded sequential workers and does not claim a general
  concurrent-worker or high-volume scheduling guarantee.

These are stated contract boundaries, not findings against the Loop 9 slice.

