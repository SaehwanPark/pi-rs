# Final report: Batch Relay live case

Date: 2026-09-20
T: Batch Relay
Branch: tester/2026-09-20-loop-4-batch-relay
Base: origin/main at e36c60e
Draft PR: https://github.com/SaehwanPark/rupi/pull/112
Endpoint: http://127.0.0.1:8000/v1 (qwen3.8-flash-next)

## Result

The bounded Batch Relay project and independent fresh-process oracle pass after
tester repair. The model did not complete T: both implementation turns timed
out before their first project write. The acceptance result is therefore
tester-authored project evidence, not model-completion evidence.

Batch Relay is incrementally harder than Webhook Inbox because it preserves
HMAC admission, SQLite persistence, direct-argv delivery, leases, crash
reclaim, and restart recovery while adding atomic dependency-DAG admission,
runnable-job scheduling, retry deferral, terminal failure, blocked dependents,
and aggregate batch status.

## Evidence summary

| Evidence | Result |
| --- | --- |
| Initial model implementation | Incomplete; session 01a0c087-2aad-7a01-9b5b-2bcbce45e86f, 2 requests, 138,062 ms, timeout, no writes |
| Recovery model implementation | Incomplete; session 01a0c089-f064-7aa5-8ccb-21bfc0a7966c, 2 requests, 127,524 ms, timeout, no writes |
| Project suite | 9 tests, OK, exit 0 |
| Independent oracle | 4 tests, OK, exit 0, 5,640 ms wrapper |
| Read-only rupi verification | Session 01a0c092-0931-75da-9bb9-b5b751e343bc, 4 process calls, 50,658 ms, exit 0 |
| Initial trace/replay | Trace 0/replay 0; 1,189 entries, no historical execution |
| Recovery trace/replay | Trace 0/replay 0; 2,411 entries, no historical execution |
| Verification trace/replay | Trace 0/replay 0; 688 entries, 4 lifecycle frames replayed |
| Rust formatting/check/clippy/tests/docs | All passed |
| Startup benchmark | Cold 140.47 ms; warm median 6.18 ms; exit 0 after Git Bash path setup |

Detailed commands, prompts, trace paths, tool outcomes, friction, and repair
boundaries are in OBSERVATIONS.md.

## Tester repair boundary

The tester added the project implementation, 9 project tests, and README under
project/ after the two model turns produced no files. The tester also made a
small case-contract correction: one --once worker invocation attempts each job
at most once, so a retryable failure remains pending for a later worker. The
independent oracle was repaired for resource cleanup and a log assertion.

No files outside docs/cases/2026-09-20-batch-relay/ changed. In particular,
there were no edits to src/, crates/, repository tests, the canonical design,
architecture, compatibility, or roadmap documents.

## Findings

1. High — local Qwen implementation turns still have an opaque first-write
   stopgate. Two fresh bounded attempts each spent one request reading the
   specification, activated the progress boundary, then timed out during the
   next model request without writing a file. The runtime recorded typed
   timeout evidence and preserved the traces, but the project could not be
   model-authored within these bounds.
2. Medium — Windows shell guidance remains imperfect. The initial model issued
   dir /b despite direct-argv guidance. This is avoidable request and latency
   cost, although it did not mutate the workspace.
3. Low — independent oracle development needed bounded repair. The first
   post-repair oracle run found same-invocation retry semantics, a log assertion
   mistake, and resource cleanup gaps. These were corrected within the case;
   they are not rupi defects.

## What worked

- HMAC verification, atomic DAG validation, idempotent/conflicting admission,
  dependency ordering, retry/terminal states, blocked dependents, leases,
  crash reclaim, restart persistence, and direct-argv sink behavior all passed
  in fresh processes.
- The project suite passed with -W error::ResourceWarning.
- The read-only rupi verification executed exactly four requested checks and
  reported their statuses without editing the workspace.
- Timed-out model sessions stayed inspectable; trace and replay exited 0 and
  replay did not re-execute recorded tools.
- The traces kept native reasoning provenance explicit, showed one active local
  model, and contained no failover or uncertain mutating operation.
- Prescribed Rust checks and the startup benchmark passed; no runtime source
  change was needed to obtain acceptance.

## Parent handoff

Recommended parent fixes:

- Keep the high implementation stopgate open. Future retries should compare
  time-to-first-write and require model-authored project-suite/oracle passes,
  with no tester repair counted as completion.
- Consider an explicit first-write or per-request progress/deadline surface that
  gives a fresh user a bounded path before a long local generation turn.
- Continue making Windows direct-argv process guidance prominent and preserve
  the useful typed provider timeout diagnostics.
- Retain the evidence discipline used here: the project suite and independent
  fresh-process oracle remain separate, and trace/replay remain read-only.

No merge or parent integration-document update was performed. The draft PR is
ready for parent review as a Loop 4 evidence handoff, not as a claim that Qwen
completed the project.
