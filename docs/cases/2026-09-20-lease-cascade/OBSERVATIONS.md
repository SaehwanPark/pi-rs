# Lease Cascade fresh-memory observations

Status: complete as a tester case; model-authoring stopgate remains open
Date: 2026-09-20  
Branch: `tester/2026-09-20-loop-6-lease-cascade`  
Base: `main` at `e450c6c`  
Draft PR: https://github.com/SaehwanPark/rupi/pull/114  
Model target: local `qwen3.8-flash-next`

This file is the chronological evidence ledger. It records exact commands,
prompts, session IDs, request counts, elapsed times, tool outcomes, friction,
repairs, and stopgates. A tester repair is never counted as model-authoring
evidence.

## Case contract

- T: Lease Cascade; see [`CASE_PLAN.md`](CASE_PLAN.md) and
  [`project/SPEC.md`](project/SPEC.md).
- Independent oracle: [`acceptance/test_lease_cascade.py`](acceptance/test_lease_cascade.py).
- Oracle boundary: it launches fresh `python -m leasecascade` service, worker,
  and sink processes and imports no project implementation module.
- Required gates: project unittest suite and independent oracle, run separately.
- Read-only requirements: `rupi trace` and `rupi replay --tools --sequence`
  must not execute tools or contact the provider.

## Exact prompt artifacts

The prompts supplied to the model were the committed files below. Their
SHA-256 hashes preserve the exact text used by the recorded invocations:

| Attempt | Prompt file | SHA-256 |
| --- | --- | --- |
| I-01 | `prompts/initial.txt` | `4B660C7927BE5021F383A4FD031519E1F042DC6C1D62F7A3EB47A0B83D66992D` |
| R-01 | `prompts/recovery.txt` | `B178BAF111CF94A38BEB0F853C3DB08032AA73AA3E98E3740A8BD836BCBDB33D` |
| V-01/V-02 | `prompts/verify.txt` | `FABA57A82A79EEFD92040A3B16CC2B8CADD1A474DD01F87B150C8272881391C1` |

Each invocation loaded its prompt with `Get-Content -LiteralPath '..\\prompts\\<name>.txt' -Raw`
from the project directory. The setup-fidelity invocation used the incorrect
`prompts/initial.txt` path and is not model evidence.

## Initial checkout and branch

Commands:

```text
git status --short --branch
git branch --show-current
git log -5 --oneline --decorate
gh auth status
git switch -c tester/2026-09-20-loop-6-lease-cascade
```

Observed base was clean `main`, synchronized with `origin/main`, at
`e450c6c`; GitHub CLI authentication was available for the authorized draft
PR handoff.

## Baseline oracle

Command, run from `docs/cases/2026-09-20-lease-cascade/` before any
`leasecascade` implementation:

```text
$sw=[Diagnostics.Stopwatch]::StartNew(); python -W error::ResourceWarning -m unittest discover -s acceptance -p 'test_*.py' -v; $code=$LASTEXITCODE; $sw.Stop(); "oracle_exit=$code elapsed_ms=$($sw.ElapsedMilliseconds)"; exit $code
```

Result: exit `1`, elapsed `10,566 ms`, `Ran 5 tests in 10.424s`, all five
failed in `setUp` because the expected implementation module was absent. The
first failure's stderr was:

```text
C:\Users\saehwan\AppData\Local\Programs\Python\Python314\python.exe: No module named leasecascade
```

This is the expected pre-implementation baseline, not an acceptance result.

## Initial PR handoff

Baseline commit: `b2cb9ff` (`test: define Loop 6 Lease Cascade case`).

Commands:

```text
git push -u origin tester/2026-09-20-loop-6-lease-cascade
gh pr create --repo SaehwanPark/rupi --base main --head tester/2026-09-20-loop-6-lease-cascade --draft --title "test: live case 6 Lease Cascade"
```

Result: push succeeded and draft PR
https://github.com/SaehwanPark/rupi/pull/114 was created. The PR body records
the pre-implementation baseline, acceptance split, and unchanged parent/runtime
scope.

## Live implementation evidence

### Setup-fidelity invocation (not model evidence)

Command run from `project/`:

```text
$prompt=Get-Content -LiteralPath 'prompts/initial.txt' -Raw; $sw=[Diagnostics.Stopwatch]::StartNew(); $result=& 'C:\Users\saehwan\repos\pi-rs\target\debug\rupi.exe' run --config rupi.config.json --cwd . --prompt $prompt --no-color --no-reasoning --verbose 2>&1; $code=$LASTEXITCODE; $sw.Stop(); 'exit_code=' + $code; 'elapsed_ms=' + $sw.ElapsedMilliseconds; '--- combined stdout/stderr ---'; $result; exit $code
```

The prompt path was wrong from the `project/` working directory, so
PowerShell reported `Cannot find path 'prompts/initial.txt'`. `rupi` therefore
received an empty prompt, made one request, created session
`01a0c0ea-133e-7885-a601-b36f83f955cb`, wrote no project files, and exited `0`
after `10,958 ms`. This is recorded as tester setup friction, not a model
attempt or completion evidence.

### I-01 corrected initial authoring

Prompt: committed `prompts/initial.txt`; config: committed
`project/rupi.config.json`; actual command from `project/`:

```text
$prompt=Get-Content -LiteralPath '..\prompts\initial.txt' -Raw; $sw=[Diagnostics.Stopwatch]::StartNew(); $result=& 'C:\Users\saehwan\repos\pi-rs\target\debug\rupi.exe' run --config rupi.config.json --cwd . --prompt $prompt --no-color --no-reasoning --verbose 2>&1; $code=$LASTEXITCODE; $sw.Stop(); 'exit_code=' + $code; 'elapsed_ms=' + $sw.ElapsedMilliseconds; '--- combined stdout/stderr ---'; $result; exit $code
```

Session: `01a0c0ea-77bf-7b0a-8be4-cb0242fbbccd`. Result: exit `1`, elapsed
`218,297 ms`; the combined stdout/stderr ended with:

```text
[model] no finish reason · 2m 00s · reasoning: reasoning
[warn] model request failed (timeout): provider request exceeded its configured total timeout (120000 ms)
[turn] timeout · 3m 38s
[session end] interrupted · provider failure: timeout: provider request exceeded its configured total timeout (120000 ms)
error: provider failure: timeout: provider request exceeded its configured total timeout (120000 ms)
```

The model made three requests. Request 1 read `SPEC.md`; the progress boundary
activated after one request without a configured progress tool. Request 2 wrote
`leasecascade/__init__.py` and `leasecascade/__main__.py` successfully. Request
3 read the remaining specification and then timed out before another write.
The trace recorded native reasoning and one active local model. The model also
used the Windows `dir /s /b` shell command for workspace inspection; it returned
paths but consumed a request/tool turn despite the prompt's direct-argv advice.
The project was not complete and neither acceptance gate was run against this
partial output.

### R-01 bounded recovery authoring

Prompt: committed `prompts/recovery.txt`; config: committed
`project/rupi.recovery.config.json`; actual command from `project/`:

```text
$prompt=Get-Content -LiteralPath '..\prompts\recovery.txt' -Raw; $sw=[Diagnostics.Stopwatch]::StartNew(); $result=& 'C:\Users\saehwan\repos\pi-rs\target\debug\rupi.exe' run --config rupi.recovery.config.json --cwd . --prompt $prompt --no-color --no-reasoning --verbose 2>&1; $code=$LASTEXITCODE; $sw.Stop(); 'exit_code=' + $code; 'elapsed_ms=' + $sw.ElapsedMilliseconds; '--- combined stdout/stderr ---'; $result; exit $code
```

Session: `01a0c0ee-e055-7674-8d7c-2c806be0cceb`. Result: exit `1`, elapsed
`128,819 ms`; two model requests. Request 1 read `SPEC.md` and executed the
Windows `dir /s /b` workspace probe. Request 2 reached the configured 120,000
ms provider timeout before writing any new file. The existing model-created
`leasecascade/__init__.py` and `__main__.py` remained the only project package
files. The session recorded native reasoning, one local active model, and the
typed timeout; neither acceptance gate passed against this partial output.

## Partial-output gates before tester repair

Project command from `project/`:

```text
$sw=[Diagnostics.Stopwatch]::StartNew(); python -W error::ResourceWarning -m unittest discover -s tests -p 'test_*.py' -v; $code=$LASTEXITCODE; $sw.Stop(); "project_exit=$code elapsed_ms=$($sw.ElapsedMilliseconds)"; exit $code
```

Result: exit `1`, elapsed `114 ms`; `unittest` reported
`ImportError: Start directory is not importable: 'tests'` because the model had
not created a test package.

Independent oracle command from the case root:

```text
$sw=[Diagnostics.Stopwatch]::StartNew(); python -W error::ResourceWarning -m unittest discover -s acceptance -p 'test_*.py' -v; $code=$LASTEXITCODE; $sw.Stop(); "oracle_exit=$code elapsed_ms=$($sw.ElapsedMilliseconds)"; exit $code
```

Result: exit `1`, elapsed `10,621 ms`; all five tests failed during fresh
server setup because `python -m leasecascade` exited with
`ModuleNotFoundError: No module named 'leasecascade.cli'`.

## Tester repair boundary

The tester implemented only inside this case directory. The model's
`__init__.py` placeholder was retained; the tester completed/revised
`__main__.py` and added `cli.py`, `ids.py`, `storage.py`, `service.py`, and
`worker.py`, plus `project/README.md`, `project/tests/__init__.py`, and
`project/tests/test_leasecascade.py`. The acceptance oracle and fake sink were
not changed after the baseline. No rupi source, rupi tests, or parent
integration documents were modified.

The first tester-authored project-suite run found one local implementation bug:
`Store.claim_next` did not select `lease_expires_at` before `_row_job` read it.
The tester added that column to the local SQL projection. This is tester repair
evidence, not model-authoring evidence.

The final implementation keeps the barrier's selected field resolution before
claim commit. A missing selected output marks the barrier failed with zero new
attempts, blocks downstream jobs, and never invokes the sink. Successful
barrier requests carry only ordered `{job_id, value}` items for the declared
field; ordinary `input_refs` remain explicit.

## Independent post-repair gates

Project gate, rerun independently after the repair:

```text
$sw=[Diagnostics.Stopwatch]::StartNew(); python -W error::ResourceWarning -m unittest discover -s tests -p 'test_*.py' -v; $code=$LASTEXITCODE; $sw.Stop(); "project_exit=$code elapsed_ms=$($sw.ElapsedMilliseconds)"; exit $code
```

Result: exit `0`, `Ran 7 tests in 0.473s`, `OK`; wrapper elapsed `598 ms`.

Independent fresh-process oracle, run separately:

```text
$sw=[Diagnostics.Stopwatch]::StartNew(); python -W error::ResourceWarning -m unittest discover -s acceptance -p 'test_*.py' -v; $code=$LASTEXITCODE; $sw.Stop(); "oracle_exit=$code elapsed_ms=$($sw.ElapsedMilliseconds)"; exit $code
```

Result: exit `0`, `Ran 5 tests in 7.251s`, `OK`; wrapper elapsed `7,397 ms`.
The oracle's standard-library AST boundary and help test passed independently;
the other tests passed through fresh service, worker, and sink processes.

Explicit help commands from `project/` also each exited `0`:

```text
python -m leasecascade --help
python -m leasecascade serve --help
python -m leasecascade worker --help
```

## Read-only trace/replay evidence

All commands below used the actual `target/debug/rupi.exe`; replay inputs were
the recorded `.trace.jsonl` files. Each replay exited `0`, emitted only
historical projections, and did not contact Qwen or execute a recorded tool.

| Session | Trace result | Replay result |
| --- | --- | --- |
| `01a0c0ea-133e-7885-a601-b36f83f955cb` setup-fidelity | `rupi trace`: 169 entries, 0 selected, exit 0 | exit 0; no tool frames |
| `01a0c0ea-77bf-7b0a-8be4-cb0242fbbccd` I-01 | `rupi trace`: 2,650 entries, 0 selected with quiet/epoch note, exit 0 | exit 0; recorded read/exec/write lifecycle only |
| `01a0c0ee-e055-7674-8d7c-2c806be0cceb` R-01 | `rupi trace`: 283 entries, 0 selected with quiet/epoch note, exit 0 | exit 0; recorded read/exec lifecycle only |
| `01a0c0f7-0785-7255-8936-b1ee6f4d9c39` V-01 | `rupi trace`: 1,233 entries, one failed `exec` shown, exit 0 | exit 0; failed `exec` and successful `grep` history only |
| `01a0c0f9-0c13-7c07-9600-032e0cd844f5` V-02 | `rupi trace`: 962 entries, one failed `ls` shown, exit 0 | exit 0; failed `ls` and successful `dir` history only |

Representative replay command form:

```text
rupi replay .rupi-state/sessions/<session-id>.trace.jsonl --tools --sequence
```

The initial and recovery exact trace/replay commands and outputs were run with
their respective `rupi.config.json`/`rupi.recovery.config.json` stores; V-01
and V-02 used `rupi.verify.config.json`.

The actual read-only command substitutions were:

```text
rupi.exe trace 01a0c0ea-133e-7885-a601-b36f83f955cb --config rupi.config.json --tools --sequence --no-color --quiet
rupi.exe replay .rupi-state\\sessions\\01a0c0ea-133e-7885-a601-b36f83f955cb.trace.jsonl --tools --sequence
rupi.exe trace 01a0c0ea-77bf-7b0a-8be4-cb0242fbbccd --config rupi.config.json --tools --sequence --no-color --quiet
rupi.exe replay .rupi-state\\sessions\\01a0c0ea-77bf-7b0a-8be4-cb0242fbbccd.trace.jsonl --tools --sequence
rupi.exe trace 01a0c0ee-e055-7674-8d7c-2c806be0cceb --config rupi.recovery.config.json --tools --sequence --no-color --quiet
rupi.exe replay .rupi-state-recovery\\sessions\\01a0c0ee-e055-7674-8d7c-2c806be0cceb.trace.jsonl --tools --sequence
rupi.exe trace 01a0c0f7-0785-7255-8936-b1ee6f4d9c39 --config rupi.verify.config.json --tools --sequence --no-color --quiet
rupi.exe replay .rupi-state-verify\\sessions\\01a0c0f7-0785-7255-8936-b1ee6f4d9c39.trace.jsonl --tools --sequence
rupi.exe trace 01a0c0f9-0c13-7c07-9600-032e0cd844f5 --config rupi.verify.config.json --tools --sequence --no-color --quiet
rupi.exe replay .rupi-state-verify\\sessions\\01a0c0f9-0c13-7c07-9600-032e0cd844f5.trace.jsonl --tools --sequence
```

## Read-only model verification

### V-01 — approval-gated verification

Command from `project/` used `prompts/verify.txt` and `rupi.verify.config.json`:

```text
$prompt=Get-Content -LiteralPath '..\prompts\verify.txt' -Raw; $sw=[Diagnostics.Stopwatch]::StartNew(); $result=& 'C:\Users\saehwan\repos\pi-rs\target\debug\rupi.exe' run --config rupi.verify.config.json --cwd . --prompt $prompt --no-color --no-reasoning --verbose 2>&1; $code=$LASTEXITCODE; $sw.Stop(); 'exit_code=' + $code; 'elapsed_ms=' + $sw.ElapsedMilliseconds; '--- combined stdout/stderr ---'; $result; exit $code
```

Session: `01a0c0f7-0785-7255-8936-b1ee6f4d9c39`. Result: exit `1`, elapsed
`97,518 ms`, three model requests, no file writes. The config's
`auto_approve_mutating: false` caused even the model's read-only `exec` attempt
to be refused by the runtime; a broad `grep` then consumed the remaining
budget over ignored `.rupi-state*` files. The model did not run the requested
checks and its incomplete recommendation is not acceptance evidence.

### V-02 — bounded config-only retry

The tester changed only the verification config to allow the prompt's
read-only `exec` commands and clarified Windows exclusions/direct argv in the
verification prompt. No project implementation or oracle file changed.

The same command form produced session
`01a0c0f9-0c13-7c07-9600-032e0cd844f5`, exit `1`, elapsed `65,970 ms`, three
model requests, and no writes. Qwen first tried `ls -la` (failed on Windows),
then `dir` (succeeded), and exhausted its request budget before running the
required tests. This is bounded model-verification friction, not evidence that
the independent gates fail.

## Verification and repairs

Final repeat after the trailing-space validation fix:

```text
$sw=[Diagnostics.Stopwatch]::StartNew(); python -W error::ResourceWarning -m unittest discover -s tests -p 'test_*.py' -v; $code=$LASTEXITCODE; $sw.Stop(); "project_exit=$code elapsed_ms=$($sw.ElapsedMilliseconds)"; exit $code
```

Exit `0`; `Ran 7 tests in 0.454s`, `OK`; wrapper elapsed `579 ms`.

```text
$sw=[Diagnostics.Stopwatch]::StartNew(); python -W error::ResourceWarning -m unittest discover -s acceptance -p 'test_*.py' -v; $code=$LASTEXITCODE; $sw.Stop(); "oracle_exit=$code elapsed_ms=$($sw.ElapsedMilliseconds)"; exit $code
```

Exit `0`; `Ran 5 tests in 7.156s`, `OK`; wrapper elapsed `7,298 ms`.

Repository verification commands, run after the case repair, produced:

| Command | Result |
| --- | --- |
| `cargo fmt --all --check` | exit `0` |
| `cargo check -p rupi-core --all-features` | exit `0`, finished in `0.10s` |
| `cargo clippy --workspace --all-targets -- -D warnings` | exit `0`, finished in `0.22s` |
| `cargo test --workspace --quiet` | exit `0`, all test binaries passed |
| `cargo doc --workspace --no-deps` | exit `0`, finished in `2.76s` |
| `bash bench/startup.sh --json /tmp/rupi-loop6-startup.json` | exit `1`; non-login WSL shell could not find `cargo` |
| `bash -lc './bench/startup.sh --json /tmp/rupi-loop6-startup.json'` | exit `0`; cold `12.59 ms`, warm mean `11.29 ms`, median `11.22 ms`, max `12.08 ms` |

Invariant review found no new rupi-runtime finding: the case changed no runtime
source or tests; traces show one local active model with native reasoning; no
backup model or model orchestration was used; replay remained historical and
read-only; and the startup result is within the documented cold/warm budgets.
The case itself preserves explicit durable state, direct-argv sink execution,
and local `Unknown`/failure handling in its project contract. The model did
not complete T, and no tester-authored file or passing gate is attributed to
the model.

## Final disposition

The independent project and oracle gates pass, but the model-authoring gate is
not met: I-01 and R-01 both stopped on the configured provider timeout before
the model produced a complete project. The implementation and focused tests
were therefore a bounded tester repair. V-01 and V-02 were also bounded and
read-only; neither is a completion claim. Parent integration documents remain
unchanged, and the draft PR remains unmerged for parent review.
