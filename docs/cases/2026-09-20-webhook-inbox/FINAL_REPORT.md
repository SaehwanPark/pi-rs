# Final report: Webhook Inbox live case

Date: 2026-09-20
T: **Webhook Inbox**
Branch: `tester/2026-09-20-loop-3-webhook-inbox`
Base: `origin/main` at `44cf075`
Draft PR: https://github.com/SaehwanPark/rupi/pull/111

## Result

Complete. Webhook Inbox is a bounded dependency-free Python service that adds
HMAC-authenticated admission and lease-expiry worker recovery to the prior
Event Outbox surface. The independent fresh-process oracle passed, including a
worker killed after a committed lease and a later reclaim with attempts
incremented. No rupi runtime/source/tests or canonical documents changed.

The live model implementation attempts did not write project files. The
tester completed the project locally under the case directory after recording
those failures as a major rupi progress stopgate. Therefore the acceptance
result is a project result, not a claim that the model turns completed T.

## Commits and changed paths

- `e4a4ceb` — plan/specification/oracle/config/ignore baseline, pushed before
  implementation; draft PR opened as #111.
- follow-up commit — project implementation, tests, observations, and report.

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

Use this exact case as the next runtime retry after adding a supported
per-request deadline/progress boundary: rerun the same initial and recovery
prompts, record time to first write and request/tool counts, and require the
model to complete the project suite and README without tester repair. Keep the
independent oracle unchanged and verify trace/replay again.
