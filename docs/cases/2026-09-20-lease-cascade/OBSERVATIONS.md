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

Pending. Each model attempt will record the exact `rupi run` command, prompt
file/content, config, session id, request count, elapsed time, stdout/stderr,
tool outcomes, and whether the first write and both acceptance gates occurred.

## Verification and repairs

Pending. Project checks, independent oracle checks, trace/replay results,
read-only verification, tester repairs, and invariant review will be added in
chronological order.
