# Final report: Webhook Inbox live case

Date: 2026-09-20
T: **Webhook Inbox**
Branch: `tester/2026-09-20-loop-3-webhook-inbox`
Base: `origin/main` at `44cf075`
Draft PR: https://github.com/SaehwanPark/rupi/pull/111

## Result

The committed Webhook Inbox project remains complete as a bounded
dependency-free Python service through the earlier tester repair, and its
independent fresh-process oracle passed. That result is not a claim that a
model turn completed T. The parent-fix retry in
[`RETRY_REPORT.md`](RETRY_REPORT.md) again produced no model-authored project
files, so the high rupi implementation stopgate remains unresolved. The final
progress-boundary retry is recorded in
[`PROGRESS_RETRY_REPORT.md`](PROGRESS_RETRY_REPORT.md): the boundary activated
and narrowed schemas, but the model still made no first write before timeout.
The final provider-effort retry is recorded in
[`REASONING_RETRY_REPORT.md`](REASONING_RETRY_REPORT.md): explicit `thinking:
low` and a 60-second deadline still produced no first write.
The final environment-only retry is recorded in
[`REASONING_OFF_RETRY_REPORT.md`](REASONING_OFF_RETRY_REPORT.md): the model
made one first write under the isolated server mode, but did not complete T.
The final one-shot progress-boundary retry is recorded in
[`ONE_SHOT_RETRY_REPORT.md`](ONE_SHOT_RETRY_REPORT.md): both turns made model
first writes, and the boundary correctly returned the normal tool set after
successful progress, but T still did not complete.
The final 120-second budget retry is recorded in
[`BUDGET_RETRY_REPORT.md`](BUDGET_RETRY_REPORT.md): the longer deadline and
six-request recovery budget produced more partial files, but T and both
acceptance checks still failed.
The recovery invocation also has a recorded prompt-fidelity deviation: one
sentence was accidentally appended beyond the committed recovery prompt. No
additional retry was run because this was the final bounded attempt.

No rupi runtime/source/tests or canonical documents changed.

## Commits and changed paths

- `e4a4ceb` — plan/specification/oracle/config/ignore baseline, pushed before
  implementation; draft PR opened as #111.
- `3ae8bb0` — project implementation, tests, observations, and report.
- `96ca4b3` — parent-fix retry configs: 30,000 ms request timeout, initial
  request limit 8, recovery request limit 3, and `thinking: off`.
- `e2e00f3` — opt-in progress boundary and Webhook Inbox progress-tool config.
- `fc56ac4` — explicit low provider effort and 60-second initial/recovery
  request deadlines.
- `20a311f` — final provider-effort retry report before the environment-only
  retry.
- `8efea83` — one-shot progress boundary after successful progress-tool
  completion.
- `5ab2dc9` — 120-second initial/recovery deadlines and six-request recovery
  budget.

All changed files are under
`docs/cases/2026-09-20-webhook-inbox/`. The runtime, Rust crates, repository
tests, canonical design, architecture, compatibility, and roadmap files are
unchanged.

## Evidence

- project suite: `Ran 11 tests in 1.610s`, `OK`, exit 0;
- independent oracle: `Ran 2 tests in 3.601s`, `OK`, exit 0;
- final read-only rupi verification: session
  `01a0c01d-aae3-70f3-90bc-6d4b1f4a9d6a`, 2 model requests, 4 successful direct
  process calls, 55.960s, exit 0;
- final verification trace: 704 entries, replay exit 0;
- interrupted implementation traces: R-01 14,216 entries/replay exit 0; R-02
  5,389 entries/replay exit 0;
- parent-fix retry traces: R-03 788 entries/replay exit 0; R-04 237
  entries/replay exit 0; neither retry wrote project files;
- final progress-boundary retry traces: R-05 214 entries/replay exit 0; R-06
  473 entries/replay exit 0; both boundaries activated, neither retry wrote
  project files;
- final provider-effort retry traces: R-07 892 entries/replay exit 0; R-08
  1,162 entries/replay exit 0; explicit low effort and 60-second deadlines
  still produced no project files;
- final environment-only retry traces: R-09 63 entries/replay exit 0; R-10
  37 entries/replay exit 0; the model wrote only `webhookinbox/__init__.py`;
  project suite and oracle both failed against the incomplete model output;
- final one-shot progress-boundary retry traces: R-11 49 entries/replay exit 0;
  R-12 542 entries/replay exit 0; both turns made model-authored first writes,
  but neither completed the package or passed the project/oracle checks;
- final 120-second budget retry traces: R-13 75 entries/replay exit 0; R-14
  1,261 entries/replay exit 0; R-13 wrote three package files and R-14 wrote
  README plus two package files, but neither completed T or passed acceptance;
- source import review found only Python standard-library modules;
- no file outside the case directory was modified.

## Findings

1. **High — repeated opaque local-generation stopgate.** R-01 spent about 13
   minutes and R-02 about 4.5 minutes without a first project write. Both were
   interrupted while generating; the tester had to repair the project locally.
2. **Medium — Windows shell friction.** R-01 issued a failed shell-shaped
   `dir /b "%CD%" && ...` command despite direct-argv guidance.
3. **Positive — trace/replay durability.** Interrupted and completed sessions
   remained inspectable, and replay stayed read-only.

## Parent retry request

The progress boundary is now verified as active, schema-narrowing, and
one-shot after successful progress: the initial trace returned from 3 schemas
to all 7 after successful writes without a second nudge/diagnostic. The high
stopgate remains because the model still did not complete the package or pass
the project/oracle checks. Any future runtime retry would need to preserve this
fresh missing-project workspace, the exact prompts, and the unchanged oracle,
and require model-authored project-suite and oracle passes without tester
repair, followed by trace/replay.

This is the final bounded retry for this case. The explicit low provider effort
and longer request deadline did not change the acceptance result.

The isolated server environment produced a first write but not a complete
model-authored project. The high stopgate remains, materially reduced but not
resolved; no further case retry is implied by this report.

The one-shot boundary fix is therefore behaviorally resolved for its targeted
reactivation bug, but the broader high implementation stopgate remains:
first-write progress is now observable, while bounded completion and acceptance
still fail. This is the final bounded retry for the case; no tester repair is
counted as implementation evidence.

The final 120-second/config-budget retry confirms that the remaining stopgate
is bounded model/provider completion rather than progress-boundary reactivation.
No further Webhook Inbox implementation retry should be performed.
