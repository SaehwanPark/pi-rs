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

### A-01 — initial model-authoring turn

Exact command, run from `project/` with stdout and stderr intentionally combined
for this first live capture:

```text
& 'C:\Users\saehwan\repos\pi-rs\target\debug\rupi.exe' run --config rupi.config.json --cwd . --prompt (Get-Content -Raw ..\prompts\initial.txt) --no-color 2>&1
```

Configuration and prompt:

- config: `project/rupi.config.json`;
- prompt: `prompts/initial.txt` (the exact committed file was passed as the
  `--prompt` argument);
- primary: `local/qwen3.8-flash-next`, native reasoning, 120,000 ms request
  timeout, eight-request turn budget, one-request no-progress boundary.

Observed result:

- session: `01a0c0b0-6ce8-767a-983a-414e1833d853`;
- trace turn duration: 244,985 ms;
- process exit: 1;
- model requests: 4 of the configured 8; the fourth request timed out after
  120,083 ms while generating reasoning and made no tool call;
- terminal diagnostic: `provider failure: timeout: provider request exceeded
  its configured total timeout (120000 ms)`; no final model answer;
- first request read `SPEC.md`; the runtime progress boundary activated after
  that inspection request and exposed the narrowed progress tool set;
- second request wrote `artifactpipe/__init__.py` successfully (893 bytes);
- third request read the missing specification region;
- fourth request produced only reasoning and timed out before another write;
- no project implementation, tests, or README were completed.

Read-only post-turn checks:

```text
& 'C:\Users\saehwan\repos\pi-rs\target\debug\rupi.exe' trace --config rupi.config.json 01a0c0b0-6ce8-767a-983a-414e1833d853 --quiet --no-reasoning
```

Result: exit 0; `4099 entries read · 1 shown`; the timeout warning was
rendered and no provider was contacted by trace.

```text
& 'C:\Users\saehwan\repos\pi-rs\target\debug\rupi.exe' replay .rupi-state\sessions\01a0c0b0-6ce8-767a-983a-414e1833d853.trace.jsonl --tools --sequence
```

Result: exit 0; replay projected only the recorded read/write/read lifecycle
at sequences 16–19, 1551–1553, and 1714–1716. It did not re-run a tool or call
the provider. The session remains incomplete evidence.

The exact emitted transcript was a combined stream; it included the original
prompt, native reasoning, tool results, the progress-boundary diagnostic, and
the typed timeout. No stdout final answer was present.

The incomplete model-authored file is preserved in the workspace and is not
counted as a completed implementation.

## Independent verification

Pending model authoring and any separately labelled tester repair.

## Trace/replay evidence

Pending live sessions. Each material session will be checked with read-only
`rupi trace` and `rupi replay --tools --sequence`; neither command may contact
the provider or execute historical tools.

## Friction, repairs, and stopgates

Pending. Slow local generation, request-budget exhaustion, or incomplete model
work will be recorded as evidence and will not be relabelled as acceptance.
