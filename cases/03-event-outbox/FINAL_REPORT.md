# Final report: Event Outbox live case

Date: 2026-09-20
T: **Event Outbox**
Branch: `tester/2026-09-20-loop-2-live-case`
Draft PR: https://github.com/SaehwanPark/rupi/pull/110
Base: `origin/main` at `7c3e9b3`
Model endpoint: local Qwen at `http://127.0.0.1:8000/v1`

## Result

Complete. Event Outbox is a bounded dependency-free Python service that adds a
durable outbox state machine, idempotent HTTP admission, retryable delivery, and
a direct-argv NDJSON sink process to the capabilities covered by Test Ledger and
Read Queue. The independent oracle passed after the tester corrected two oracle
command/ordering mistakes; no rupi runtime changes were needed.

## Commits and changed paths

- `c8ff2cb` — first specification/oracle commit, pushed before implementation;
  created `CASE_PLAN.md`, `project/SPEC.md`, the independent oracle and fake
  sink, `.gitignore`, and the initial Qwen config.
- `5047573` — follow-up implementation, oracle corrections, configs, and
  observations/evidence.
- The branch-tip documentation commit finalizes this report and contains no
  code changes.

All changes are under `docs/cases/2026-09-20-event-outbox/`:

- `project/outbox/` — CLI, HTTP server, SQLite store, validation, and worker;
- `project/tests/` — six focused project tests;
- `acceptance/` — two fresh-process tests and independent sink;
- `project/README.md`, three run-specific configs, `CASE_PLAN.md`,
  `OBSERVATIONS.md`, and this report.

## Evidence

- Project suite: `Ran 6 tests in 0.449s`, `OK`, exit `0`.
- Independent fresh-process HTTP/worker/restart oracle: `Ran 2 tests in
  2.644s`, `OK`, exit `0`.
- Final read-only rupi verification: session
  `01a0bfb7-6897-72c8-a50f-c08b99651c14`, 2 model requests, 4 successful direct
  process calls, 78.687 seconds, exit `0`.
- Final `rupi trace`: exit `0`; final `rupi replay --tools --sequence`: exit
  `0`. The incomplete initial trace and replay also remained readable.
- No Rust, `src/`, `crates/`, canonical design, compatibility, or roadmap files
  changed.

## Ranked friction

1. High: local Qwen implementation turns spent 26m 30s and 7m 20s without
   producing the project; one length-limited turn still reported exit `0` and
   completed status.
2. Medium: Windows shell quoting and `dir` exploration consumed failed/avoidable
   tool calls despite direct-argv guidance.
3. Medium: read-only process verification still requires mutation approval in a
   trusted workspace.
4. Low: option-like sink arguments need `--sink-arg=--log`; the oracle and README
   now show this exact form.

## Major rupi issue

Yes. The major remaining issue is opaque local-turn progress and completion
semantics around output-length boundaries: R-02 has a reproducible
`finish_reason=length` with no implementation but exit `0` and
`turn_completed=completed`. It remains open as a runtime/UX finding; this case
does not claim it was fixed.
