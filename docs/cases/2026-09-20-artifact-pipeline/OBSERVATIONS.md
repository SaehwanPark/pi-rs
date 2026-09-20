# Observations: Artifact Pipeline fresh-memory case

Date: 2026-09-20
Operator: Codex acting as a fresh `rupi` user
Branch: `tester/2026-09-20-loop-5-artifact-pipeline`
Base: `origin/main` at `959fe7e`
Draft PR: https://github.com/SaehwanPark/rupi/pull/113
Model target: local `qwen3.8-flash-next`
Endpoint: `http://127.0.0.1:8000/v1`

This is a chronological evidence log. Commands, prompts, session IDs, request
counts, elapsed times, stdout/stderr outcomes, tool outcomes, friction, repairs,
and stopgates are recorded as they occur. A model summary is never used as
acceptance evidence.

## Case contract

- T: Artifact Pipeline; see [`CASE_PLAN.md`](CASE_PLAN.md) and
  [`project/SPEC.md`](project/SPEC.md).
- Independent oracle: [`acceptance/test_artifact_pipeline.py`](acceptance/test_artifact_pipeline.py).
- Oracle implementation boundary: it launches fresh `python -m artifactpipe`,
  worker, and sink processes and imports no project implementation modules.
- Configs: `project/rupi.config.json`, `project/rupi.recovery.config.json`,
  `project/rupi.verify.config.json`.
- Exact prompts: [`prompts/`](prompts/).
- Acceptance gates: project unittest suite and independent oracle must each pass;
  trace/replay must remain read-only.

## Initial state and branch

- `main` was clean and matched `origin/main` at `959fe7e`.
- Local Qwen was available and reported model alias `qwen3.8-flash-next`.
- `target/debug/rupi.exe` was present before live work.
- Branch was created from current `main` and pushed before model work.
- Draft PR #113 was opened before model work.

## Baseline oracle — before implementation

Command, run from `docs/cases/2026-09-20-artifact-pipeline/`:

```text
python -W error::ResourceWarning -m unittest discover -s acceptance -p 'test_*.py' -v
```

Result:

- elapsed: 33,390 ms;
- exit: 1;
- tests: 4, all failed in `setUp` because the expected `artifactpipe` server
  could not start (`WinError 10061`); the missing implementation was the
  intended pre-authoring failure;
- no project files, SQLite database, or sink process were created.

## Live attempts

No live model attempt has been run yet. This section will list each bounded
`rupi run` invocation with its exact prompt file, session ID, request count,
elapsed time, terminal status, tool lifecycle outcomes, and resulting files.

## Independent verification

Pending model authoring and any separately labelled tester repair.

## Trace/replay evidence

Pending live sessions. Each material session will be checked with read-only
`rupi trace` and `rupi replay --tools --sequence`; neither command may contact
the provider or execute historical tools.

## Friction, repairs, and stopgates

Pending. Slow local generation, request-budget exhaustion, or incomplete model
work will be recorded as evidence and will not be relabelled as acceptance.

