# Final report: Lease Receipt live case

Date: 2026-09-20
T: **Lease Receipt**
Base: `main` at `1e4030d`
Branch: `tester/2026-09-20-loop-8-lease-receipt`
Draft PR: https://github.com/SaehwanPark/rupi/pull/116

## Result

Lease Receipt passes both independent acceptance gates after tester repair. It
is one focused step beyond Lease Fence: it retains private stale-claim fencing
and adds stable non-secret delivery keys plus durable sink receipts across a
lost acknowledgement/worker crash. A fresh worker can recover one logical sink
application without pretending that arbitrary sinks provide exactly-once
delivery.

This is not model-completed evidence. Both bounded Qwen authoring turns stopped
at provider timeouts before writing implementation files; all functional case
files are tester-authored.

## Acceptance

| Gate | Result |
| --- | --- |
| Project suite | `python -W error::ResourceWarning -m unittest discover -s tests -p 'test_*.py' -q`; 8 tests, exit 0, 1.670 s |
| Independent fresh-process oracle | `python -W error::ResourceWarning -m unittest discover -s acceptance -p 'test_*.py' -q`; 5 tests, exit 0, 7.665 s |

The oracle checks HMAC/atomic admission, idempotency/conflict, ordered
fan-out/barrier data flow, retry/terminal/blocked states, restart persistence,
lease reclaim, stale-worker non-overwrite, direct argv, standard-library-only
imports, and receipt replay after a killed worker. The receipt race observed one
applied side effect, one replayed receipt, the same `receipt:source` key, and
attempts equal to two.

## Model attempts

| Attempt | Session | Budget/result |
| --- | --- | --- |
| I-01 initial | `01a0c13d-ca84-7001-a696-0a32253c1059` | 6-request budget; 2 requests, 101,485 ms, exit 1; second request timed out at 90 s; no project files |
| R-01 recovery | `01a0c13f-9f41-74e4-b3f6-624171aae11d` | 4-request budget; 2 requests, 101,988 ms, exit 1; second request timed out at 90 s; no project files |

The verified local endpoint was HTTP 200 at `/v1/models` in 69 ms. No verify
model turn was needed. The model traces show one local model, native reasoning,
zero failover, and zero unknown tool events.

## Repairs and commits

- `857fc0d` — case plan/spec/oracle/config/prompt baseline.
- `2d040c3` — oracle-only fixture/assertion corrections after the baseline
  run exposed logging and expectation defects.
- `addd74b` — tester-authored standard-library project, README, and eight
  project tests after model stopgates.
- `0dc40e1` — oracle-only blocking-sink cleanup; follow-up left zero helper
  processes.
- Current final evidence adds `INVARIANT_REVIEW.md`, `OBSERVATIONS.md`,
  `FINAL_REPORT.md`, and `startup-loop8.json`.

All files are under `docs/cases/2026-09-20-lease-receipt/`; protected rupi
source, repository tests, canonical docs, roadmap, README, and parent
integration documents are untouched.

## Trace, replay, and invariant findings

`rupi trace` and `rupi replay --tools --sequence` exited 0 for both model
sessions. Trace read 1,286 entries for I-01 and 1,234 for R-01; replay emitted
only recorded read/exec tool activity and did not generate new work. The
three-pass invariant review is `pass` with no actionable findings. The main
residual risk is deliberate: delivery-key receipts are an idempotent-sink
protocol, not a universal exactly-once guarantee.

## Repository checks

`cargo fmt --all --check`, `cargo check -p rupi-core --all-features`, clippy
with warnings denied, `cargo test --workspace`, and `cargo doc --workspace
--no-deps` passed. The startup benchmark passed via `bash -lc` with cold
11.058 ms and warm median 10.949 ms.

## Recommendation for parent integration

Treat Lease Receipt as a valid Loop 8 tester fixture and a tester-repaired
acceptance result while keeping the model-authoring stopgate visible. Review
whether stable logical delivery identity versus private claim identity should
inform a future generic runtime contract or compatibility fixture. Preserve the
bounded distinction between retryable/lost acknowledgement, `Unknown` side
effect state, and exactly-once claims; do not count this case as evidence that
arbitrary external sinks are exactly once.

