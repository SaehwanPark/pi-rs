# Final report: Artifact Pipeline live case

Date: 2026-09-20
T: Artifact Pipeline
Branch: `tester/2026-09-20-loop-5-artifact-pipeline`
Base: `origin/main` at `959fe7e`
Draft PR: https://github.com/SaehwanPark/rupi/pull/113

## Status

Acceptance passed after bounded tester-authored repair. Neither model-authoring
turn completed T, so this is not model-completed evidence.

## Result

Artifact Pipeline is a valid next-step fixture beyond Batch Relay: its project
suite and independent fresh-process oracle both pass. The result is explicitly
split between model evidence and tester repair:

| Evidence | Session / command | Result |
| --- | --- | --- |
| A-01 initial model authoring | `01a0c0b0-6ce8-767a-983a-414e1833d853`; 4 requests; 244,985 ms | exit 1; fourth request timed out; only a package placeholder was written |
| A-02 bounded recovery authoring | `01a0c0b5-6491-79ec-8bf7-9ae7546e1f17`; 6 requests; 203,505 ms | exit 1; request budget exhausted; no functional implementation |
| Tester repair | case-local `project/` and independent oracle only | functional implementation and tests added; not model completion |
| A-03 read-only verification | `01a0c0c1-b5e5-7006-8c57-f355a94ee7e0`; 3 requests; 57,475 ms | exit 1; inspection-only model run exhausted its budget; no files changed |
| Project gate | `python -W error::ResourceWarning -m unittest discover -s tests -p 'test_*.py' -v` | exit 0; 6 tests; 0.387 s |
| Independent oracle | `python -W error::ResourceWarning -m unittest discover -s acceptance -p 'test_*.py' -v` | exit 0; 4 tests; 5.560 s |
| Trace/replay | `rupi trace` and `rupi replay --tools --sequence` for A-01/A-02/A-03 | all exit 0; read-only projections; no tool/provider execution |

The oracle checks signed admission, atomic idempotency/conflict handling,
declared dependency references, dependency order, output-to-input propagation,
retryable/terminal/blocked states, lease reclaim, restart persistence, direct
argv worker execution, malformed input, standard-library-only boundary, and
secret non-persistence. It imports no project implementation module.

## Tester repair boundary

The tester wrote `ids.py`, `storage.py`, `service.py`, `worker.py`, and
`__main__.py`; completed `README.md` and focused project tests; and strengthened
the independent oracle. The model-created `artifactpipe/__init__.py` placeholder
is preserved but is not credited as implementation. A first local test run
identified and fixed open SQLite connections and a test-selection error. No
rupi source, rupi tests, or parent integration documents were modified.

## Findings

- **High — model-authoring stopgate.** Local Qwen generation was too slow and
  consumed the bounded budgets before implementation. The live evidence should
  not be reported as a successful model build.
- **Medium — cross-platform command friction.** The model used Unix `ls` in a
  Windows workspace and spent recovery requests on directory probing. This
  reduced the useful request budget but did not demonstrate a rupi runtime bug.
- **Low — no core regression observed.** The required Rust format, check,
  clippy, workspace test, documentation, and startup checks passed. Startup
  reported 9.71 ms cold and 9.15 ms warm median in the corrected environment.

## Recommended action

Keep the model-authoring stopgate open: classify this case as tester-repaired
acceptance evidence, not model-completed evidence. Preserve the independent
oracle and its repair boundary for parent comparison with the earlier ladder.
There is no evidence in this slice that warrants weakening rupi semantics or
changing trace/replay behavior. A future improvement may reduce Windows shell
friction or make the first functional slice more likely within the same model
budget, but that should be tested separately.

The WIP PR remains draft and unmerged. Parent integration documents and the
roadmap were intentionally left unchanged.
