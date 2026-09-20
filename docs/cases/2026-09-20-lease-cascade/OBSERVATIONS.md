# Lease Cascade fresh-memory observations

Status: in progress  
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

## Verification and repairs

Pending. Project checks, independent oracle checks, trace/replay results,
read-only verification, tester repairs, and invariant review will be added in
chronological order.
