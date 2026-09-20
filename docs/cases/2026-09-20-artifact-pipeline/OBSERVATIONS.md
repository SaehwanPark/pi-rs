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

### A-02 — bounded recovery model-authoring turn

Exact command, run from `project/` with stdout and stderr combined:

```text
& 'C:\Users\saehwan\repos\pi-rs\target\debug\rupi.exe' run --config rupi.recovery.config.json --cwd . --prompt (Get-Content -Raw ..\prompts\recovery.txt) --no-color 2>&1
```

Configuration and prompt:

- config: `project/rupi.recovery.config.json`;
- prompt: `prompts/recovery.txt`;
- fresh state directory: `.rupi-state-recovery`;
- six-request turn budget, one-request no-progress boundary, 120,000 ms
  provider request timeout.

Observed result:

- session: `01a0c0b5-6491-79ec-8bf7-9ae7546e1f17`;
- trace turn duration: 203,505 ms;
- process exit: 1;
- six model requests completed; the turn ended with
  `turn aborted: model request budget exhausted` and an incomplete finalization
  answer;
- first tool was a Windows `cmd.exe`-shaped directory probe, which succeeded
  but cost a mutating `exec` call and listed rupi state files;
- the model wrote a one-line `README.md` placeholder and a temporary
  `_tmp_SPEC_capture.md`, then removed the temporary file; it did not write
  implementation modules or tests;
- the final model text accurately stated that the task was incomplete; no
  acceptance command was run and no functional project was produced.

Read-only post-turn checks:

```text
& 'C:\Users\saehwan\repos\pi-rs\target\debug\rupi.exe' trace --config rupi.recovery.config.json 01a0c0b5-6491-79ec-8bf7-9ae7546e1f17 --quiet --no-reasoning
```

Result: exit 0; `1677 entries read · 1 shown`; the request-budget diagnostic
was rendered without contacting the provider.

```text
& 'C:\Users\saehwan\repos\pi-rs\target\debug\rupi.exe' replay .rupi-state-recovery\sessions\01a0c0b5-6491-79ec-8bf7-9ae7546e1f17.trace.jsonl --tools --sequence
```

Result: exit 0; replay projected the recorded directory `exec`, two README/
temporary-file writes, subsequent reads/exec, and no unrecorded historical
execution. The model-authored project remains incomplete evidence.

## Tester repair boundary

Both model-authoring attempts stopped before producing a functional project, so
tester repair was required to evaluate T itself. Repair began only after the
A-01 and A-02 trace/replay checks and stayed inside the case directory.

Tester-authored implementation files:

- `project/artifactpipe/ids.py`: strict pipeline/job shape validation,
  dependency-cycle checks, declared reference checks, and canonical identity;
- `project/artifactpipe/storage.py`: SQLite schema, atomic admission and
  idempotency/conflict handling, leases, reclaim, retries, blocked propagation,
  persisted outputs, and declared input resolution;
- `project/artifactpipe/service.py`: HMAC-authenticated HTTP admission/status
  routes and health endpoint;
- `project/artifactpipe/worker.py`: direct-argv subprocess execution, one-line
  JSON request/response, leases, dependency ordering, and retry/terminal
  handling;
- `project/artifactpipe/__main__.py`: `serve` and `worker` CLI entry points;
- `project/README.md` and `project/tests/test_artifactpipe.py`: project
  documentation and six focused unit tests.

The tester also strengthened the independent oracle in
`acceptance/test_artifact_pipeline.py` with malformed-JSON coverage, a
standard-library-only AST boundary check, and a persisted-secret check. The
model-created `project/artifactpipe/__init__.py` placeholder remains a
model-authored artifact, but is not treated as a completed model
implementation. No rupi source, runtime tests, parent integration document, or
trace was edited during repair.

The first post-repair project run exposed two tester defects, not rupi defects:
SQLite connections were left open on Windows (raising `ResourceWarning` and
locking the database), and one test selected the earlier retry pipeline when it
intended the terminal-failure pipeline. The connection lifecycle and test
selection were corrected before the final gates below.

### A-03 — read-only model verification after repair

This was a fresh rupi process with a separate state directory and a prompt that
asked the model only to inspect the finished case. It was not an acceptance
authoring attempt.

Exact command, run from `project/` with stdout and stderr combined:

```text
& 'C:\Users\saehwan\repos\pi-rs\target\debug\rupi.exe' run --config rupi.verify.config.json --cwd . --prompt (Get-Content -Raw ..\prompts\verify.txt) --no-color 2>&1
```

Observed result:

- session: `01a0c0c1-b5e5-7006-8c57-f355a94ee7e0`;
- fresh state directory: `.rupi-state-verify`;
- trace turn duration: 57,475 ms;
- process exit: 1; three requests consumed and the turn ended with the
  configured model-request budget exhausted;
- the first request tried Unix `ls -la` and failed on Windows; the second used
  `dir /b` successfully; the third produced only an incomplete finalization
  response; no write, edit, or append tool was requested;
- no project file changed during this verification run.

Read-only trace:

```text
& 'C:\Users\saehwan\repos\pi-rs\target\debug\rupi.exe' trace --config rupi.verify.config.json 01a0c0c1-b5e5-7006-8c57-f355a94ee7e0 --quiet --no-reasoning
```

Result: exit 0; `811 entries read · 2 shown[in 36 ms, state failed] · 37 ms ·
exit 1`. The command only rendered the failed `exec` and budget warning.

Read-only replay:

```text
& 'C:\Users\saehwan\repos\pi-rs\target\debug\rupi.exe' replay .rupi-state-verify\sessions\01a0c0c1-b5e5-7006-8c57-f355a94ee7e0.trace.jsonl --tools --sequence
```

Result: exit 0; replay projected the requested/failed `exec` at sequences
40–42 and the later successful directory `exec` at sequences 62–64. It did
not execute either command and did not contact the provider.

## Independent verification

### Project gate

From `docs/cases/2026-09-20-artifact-pipeline/project/`:

```text
python -W error::ResourceWarning -m unittest discover -s tests -p 'test_*.py' -v
```

Result: exit 0; `Ran 6 tests in 0.387s`, `OK`. The six tests cover validation,
idempotent/conflicting admission, output resolution, retry/terminal/blocked
state, and CLI help.

Additional project checks:

```text
python -m compileall -q artifactpipe tests
python -m artifactpipe --help
python -m artifactpipe serve --help
python -m artifactpipe worker --help
```

All four commands exited 0; compileall was silent and each help command
printed its expected usage without starting a server or worker.

### Independent fresh-process oracle

From `docs/cases/2026-09-20-artifact-pipeline/`:

```text
python -W error::ResourceWarning -m unittest discover -s acceptance -p 'test_*.py' -v
```

Result: exit 0; `Ran 4 tests in 5.560s`, `OK`. The oracle launched fresh server,
worker, and sink processes, and independently checked signed admission,
idempotency/conflict behavior, declared artifact references, dependency order,
output propagation, retryable/terminal/blocked states, lease reclaim, restart
persistence, CLI help, malformed JSON, standard-library-only implementation
imports, and absence of the test secret from SQLite bytes. It imports no
`artifactpipe` project implementation module.

### Trace/replay safety

All three material sessions were checked with read-only `rupi trace` and
`rupi replay --tools --sequence`: A-01, A-02, and A-03. Each command exited 0
and projected recorded lifecycle data only. None executed a recorded tool or
contacted the provider. No blind replay was used for repair or acceptance.

## Repository verification

The required repository checks were run after the case repair; none changed
rupi source or tests:

```text
cargo fmt --all --check
cargo check -p rupi-core --all-features
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo doc --workspace --no-deps
bash bench/startup.sh --json bench/results/startup-ci.json
```

Results:

- `cargo fmt --all --check`: exit 0;
- `cargo check -p rupi-core --all-features`: exit 0, 1.10 s;
- `cargo clippy --workspace --all-targets -- -D warnings`: exit 0, 2.20 s;
- `cargo test --workspace`: exit 0; every workspace unit, integration, and
  doc-test suite passed with zero failures;
- `cargo doc --workspace --no-deps`: exit 0, 3.39 s;
- the first startup invocation from the non-login shell failed before building
  with `bench/startup.sh: line 44: cargo: command not found`; this was a shell
  environment issue, not a rupi result;
- the bounded environment-corrected rerun was:

  ```text
  bash -lc 'bash bench/startup.sh --json bench/results/startup-ci.json'
  ```

  It exited 0 and reported cold 9.71 ms; warm 10-run min 8.69 ms, mean 9.15
  ms, median 9.15 ms, max 10.02 ms. The JSON output is ignored by
  `bench/results/.gitignore` and was not committed.

## Friction, repairs, and stopgates

- **High — model-authoring stopgate:** A-01 timed out after four requests and
  244,985 ms; A-02 exhausted six requests after 203,505 ms. The two-attempt
  recovery budget was honored. Neither model turn completed T, and neither is
  counted as model-authored acceptance.
- **Medium — Windows shell friction:** the model spent requests on Unix-style
  `ls` and cmd-shaped directory probing. A-03 reproduced the `ls` failure even
  after tester repair. This is useful user friction, but no rupi runtime defect
  was established.
- **Low — tester-local repair friction:** the independent project suite caught
  open SQLite connections and a test-selection mistake. Both were fixed in the
  case-local project before the final independent gates.
- **No rupi blocker observed:** the runtime preserved the active-model budget,
  progress boundary, durable traces, and read-only trace/replay behavior. No
  rupi source change was made to make T pass.

Invariant review for this slice:

- one active model was used per live run; no backup model or orchestration was
  introduced;
- canonical trace evidence was kept separate from model summaries and no
  hidden reasoning recovery was claimed;
- mutating model tool calls were never replayed, and no uncertain mutation was
  blindly retried;
- the independent oracle uses fresh processes and no project implementation
  imports;
- startup behavior was not changed because no rupi runtime path was modified.
