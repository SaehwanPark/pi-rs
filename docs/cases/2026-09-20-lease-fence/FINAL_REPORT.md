# Final report: Lease Fence live case

Date: 2026-09-20
T: **Lease Fence**
Branch: `tester/2026-09-20-loop-7-lease-fence`
Base: `main` at `d98cf8d`
Draft PR: https://github.com/SaehwanPark/rupi/pull/115
Model endpoint: `http://127.0.0.1:8000/v1` (`qwen3.8-flash-next`)

## Result

Lease Fence passes both independent acceptance gates after a bounded tester
repair. It is one focused step beyond Lease Cascade: it preserves the prior
signed pipeline/barrier/lease/reclaim contract and adds private per-claim
fencing so a stale worker response cannot overwrite a newer reclaimed claim.

This is not model-completed evidence. The initial and recovery model turns
both stopped before functional implementation; the project and oracle are
explicitly tester-authored acceptance evidence.

## Acceptance

| Gate | Result |
| --- | --- |
| Project suite | `python -W error::ResourceWarning -m unittest discover -s tests -p 'test_*.py' -v`; 7 tests, exit 0, 1.478 s |
| Independent fresh-process oracle | `python -W error::ResourceWarning -m unittest discover -s acceptance -p 'test_*.py' -v`; 4 tests, exit 0, 5.268 s |

The oracle starts separate service, worker, and sink processes and independently
checks HMAC authentication, atomic/idempotent/conflicting admission, ordered
fan-out and barrier fan-in, declared output references, retryable and terminal
failures, blocked dependents, restart persistence, lease reclaim, stale-worker
non-overwrite, direct argv, help output, and standard-library-only imports.

## Model attempts

| Attempt | Session | Budget/result | Evidence |
| --- | --- | --- | --- |
| I-01 initial | `01a0c11c-6f16-7954-9d47-e8ba499bafaa` | 6 requests; 204,948 ms; exit 1; budget exhausted | Model wrote only `leasefence/__init__.py`; no functional project or tests |
| R-01 recovery | `01a0c11f-be3d-7baf-9a44-bb8d5a640697` | 3 requests; 169,788 ms; exit 1; 90 s request timeout | No functional additions; failed README edit, then provider timeout |

The local endpoint was independently healthy before authoring. Both traces show
one active local model and native reasoning provenance, with no failover or
unknown tool event. Exact prompts, commands, tool outcomes, and hashes are in
[`OBSERVATIONS.md`](OBSERVATIONS.md).

## Repair and commits

All files remain under `docs/cases/2026-09-20-lease-fence/`:

- `project/leasefence/` — tester-completed standard-library service, SQLite
  store, HTTP boundary, CLI, worker, and private token fencing;
- `project/tests/` — seven focused tests including old-token rejection;
- `project/README.md` and `project/SPEC.md`;
- `acceptance/test_lease_fence.py` and `acceptance/fake_sink.py`;
- `CASE_PLAN.md`, prompts, configs, `OBSERVATIONS.md`, and this report.

The durable branch trail is:

- `d0c497c` — case plan/spec/oracle/config/prompt baseline;
- `a7678bd` — oracle-only closed-pipe cleanup after the expected missing-module
  baseline exposed `ResourceWarning` noise;
- `b82c378` — tester-local implementation, project tests/README, and final
  oracle assertion.

No rupi source, repository tests, canonical docs, roadmap, README, user
manual, or parent integration document was changed.

## Trace/replay and invariant findings

`rupi trace` and `rupi replay --tools --sequence` exited 0 for both material
model sessions. Replay emitted only recorded history; it did not invoke Qwen or
re-execute reads, directory probes, writes, or failed tool calls. The traces
show one local model with native provenance, no backup activation, no model
orchestration, and no unknown tool completion.

The case-local implementation keeps durable SQLite state authoritative for sink
effects. Reclaim clears the old token; finalization requires the exact token and
leased status. A stale response is rejected and reported, not converted to a
success or replayed. The new concurrency check is intentionally bounded and
does not claim exactly-once external delivery.

## Repository verification

`cargo fmt --all --check`, `cargo check -p rupi-core --all-features`, clippy
with warnings denied, all workspace tests, workspace docs, and the startup
benchmark passed. Startup measured 14.99 ms cold and 10.24 ms warm median.

## Recommendation for parent integration

Treat this as a valid Loop 7 tester fixture and a tester-repaired acceptance
result, while keeping the model-authoring stopgate visible. The parent should
review whether the private claim-token/fenced-finalization behavior should
inform a future generic runtime contract or compatibility fixture. Separately,
the repeated model evidence supports continued investigation of bounded
first-write progress, Windows shell friction, and provider-timeout UX; this
case does not justify weakening rupi semantics or counting tester repair as
model completion.

The draft PR remains intentionally unmerged for parent review. Parent
integration documents remain untouched.
