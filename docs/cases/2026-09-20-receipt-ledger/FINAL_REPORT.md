# Loop 9 fresh-memory tester report

## Result

**Receipt Ledger** is the tenth project target and the capstone case passes
both gates after a bounded, explicitly recorded tester repair. Model authoring
did not complete the project and is not being represented as completion.

Receipt Ledger is incrementally harder than Lease Receipt by one bounded
dimension: a standard-library SQLite audit ledger that records authoritative
admission/job transitions in the same transaction and verifies a canonical
SHA-256 hash chain in a fresh read-only process. It does not add a scheduler,
framework, migration system, concurrent-worker guarantee, or exactly-once
claim.

## Branch and PR

- Branch: `tester/2026-09-20-loop-9-receipt-ledger`
- Base: `main` at `8d234cb`
- Draft WIP PR: https://github.com/SaehwanPark/rupi/pull/117
- Case files: `docs/cases/2026-09-20-receipt-ledger/`

## Model attempts

- Initial: local `qwen3.8-flash-next`, persistent session
  `01a0c165-0af3-77da-9d22-2202f2d6b75c`, 6 requests, 206,145 ms wrapper
  time, exit 1. It wrote only `project/receiptledger/__init__.py` and
  `__main__.py`; the sixth request ended incomplete at the fixed budget.
- Recovery: local `qwen3.8-flash-next`, persistent session
  `01a0c168-5615-763a-a5f3-32a0a4107f7d`, 2 requests, 105,559 ms wrapper
  time, exit 1. The first Windows-incompatible POSIX probe failed and the
  second request hit the 90,000 ms provider timeout.
- Verify-model phase: not run. The fixed authoring stopgates were exhausted,
  and the required evidence was obtained through the allowed bounded tester
  repair and independent gates.

Prompt/config hashes, exact commands, per-request timings, tool outcomes, and
session paths are recorded in `OBSERVATIONS.md`.

## Tester repair

The tester implemented the remaining standard-library project only under the
case directory: `project/receiptledger/{audit,cli,ids,service,storage,worker}.py`,
`project/README.md`, project tests, and the startup result. Two harness cleanup
edits close SQLite connections under Windows. One safe audit detail was renamed
from `claim_token_mismatch` to `stale_claim_rejected` after the strict oracle
found that even the private-token substring leaked into an audit detail. The
oracle assertion stayed strict. These changes are tester repair, not model
completion.

## Gates

| Gate | Command/result |
| --- | --- |
| Pre-implementation oracle | 4 import failures, exit 1, 6,433 ms; expected missing-package baseline |
| Project suite | `python -W error::ResourceWarning -m unittest discover -s tests -p 'test_*.py' -q`: 9 passed, exit 0, 1.642 s internal |
| Independent oracle | `python -W error::ResourceWarning -m unittest discover -s acceptance -p 'test_*.py' -q`: 4 passed, exit 0, 7.150 s internal |
| Repository checks | fmt, core check, clippy `-D warnings`, workspace tests, docs, and `git diff --check`: all pass |
| Startup | cold 9.736 ms; warm mean 9.255 ms over 10 iterations: pass |

The fresh-process oracle covers auth and atomic admission, idempotent/conflict
behavior, declared dependency/barrier flow, retry/terminal/blocked state,
lease reclaim and stale fencing, lost-ack receipt replay using the same stable
delivery key, audit verification after restart, no secret/token/argv leakage,
direct argv, help, standard-library-only imports, and independent tamper
detection.

## Trace, replay, and invariant result

Read-only trace/replay inspection exited 0 for both sessions. Initial trace
read 1,036 entries and recovery trace read 1,607 entries; replay showed only
the recorded model read/write/failed-tool lifecycles and did not execute a
historical tool. No backup model was activated. `INVARIANT_REVIEW.md` is a
pass: state authority, Unknown/lost-ack behavior, stable public delivery key,
private fencing, declared data flow, audit transaction/hash integrity, direct
argv, lazy startup, native provenance, and case-only scope all hold.

## Commits and integration recommendations

- `9e704fa` — `test: define Loop 9 Receipt Ledger case`
- `d5fa3c1` — `test: repair Loop 9 Receipt Ledger project`
- The final evidence commit contains `OBSERVATIONS.md`,
  `INVARIANT_REVIEW.md`, and this `FINAL_REPORT.md`.

Recommended parent integration:

1. Preserve the audit ledger as operational evidence, separate from rupi's
   canonical trace and model-visible context; it must never drive work.
2. Preserve the safe-field allowlist, same-transaction append, independent
   verification, private fencing, explicit Unknown/lost-ack handling, and
   at-least-once/not-exactly-once boundary.
3. Integrate the case evidence only after reviewing the draft PR; do not infer
   model completion from the final repaired project.
