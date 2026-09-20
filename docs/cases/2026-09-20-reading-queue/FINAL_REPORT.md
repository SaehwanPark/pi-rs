# Tester report: Read Queue

## Result

T was a small dependency-free Python HTTP/SQLite reading-queue service. It was
selected as the next difficulty step after Test Ledger because it adds a
long-lived process, HTTP boundaries, relational persistence, restart recovery,
and fresh-process integration testing without becoming a broad application.

The specification is [project/SPEC.md](project/SPEC.md). Implementation is
[project/readqueue/](project/readqueue/), with usage in
[project/README.md](project/README.md). The independent oracle is
[acceptance/test_readqueue_http.py](acceptance/test_readqueue_http.py).
Detailed evidence and friction are in [OBSERVATIONS.md](OBSERVATIONS.md).

## Branch and PR

- Branch: `tester/2026-09-20-next-live-case`
- Draft PR: https://github.com/SaehwanPark/rupi/pull/108
- Commits: `982d884` (spec/oracle/config), `17e81ca` (implementation,
  verification evidence, and tester report).
- No merge performed.

## Exact results

- Initial `rupi run`: session `01a0bf2e-33cf-7890-9073-9d8b25ff3a6b`, 24 model
  requests, 2,491,586 ms, `budget_exhausted`, exit 1; implementation was
  incomplete.
- Fresh `rupi run` verification: session
  `01a0bf58-2f36-75a6-8915-0c2a5d2831a5`, 3 requests, 113,716 ms, completed,
  exit 0; 67 project tests and both help commands passed.
- Final project suite: `Ran 67 tests in 5.467s`, `OK`, exit 0.
- Final independent fresh-process/restart oracle: `Ran 1 test in 1.197s`,
  `OK`, exit 0.
- `rupi trace` and `rupi replay --tools --sequence` for the initial session:
  both exit 0.

## Ranked findings and retry plan

1. High: the initial turn exhausted its entire request budget before testing;
   split implementation from verification and reserve an early test checkpoint.
2. High: Qwen rejected `thinking: minimal` (`xhigh`, `medium`, `low` only),
   while rupi surfaced only blank `provider_unavailable` detail; validate
   endpoint-specific thinking values and preserve bounded provider diagnostics.
3. Medium: Windows `ls`/shell quoting caused avoidable failed tool calls; steer
   toward direct argv process execution.
4. Medium: cross-file defects survived until independent testing; require exact
   test/help output before finalization.

The T is complete after minimal project-local fixes. The parent should retry the
same case after addressing the first two runtime issues, then compare request
count, completion status, and whether model-authored verification passes without
manual repair.
