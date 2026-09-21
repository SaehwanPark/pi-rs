# Webhook Inbox live-case observations

Date: 2026-09-20
Operator: Codex acting as a fresh `rupi` user
Host: Windows PowerShell, Python 3.14.7
Branch: `tester/2026-09-20-loop-3-webhook-inbox`
Base: clean `origin/main` at `44cf075` (`test: live case 2 Event Outbox (#110)`)
Draft PR: https://github.com/SaehwanPark/rupi/pull/111
Endpoint: `http://127.0.0.1:8000/v1`, model `qwen3.8-flash-next`

## Selected project

T is **Webhook Inbox**, a dependency-free Python HTTP/SQLite service plus a
separate worker. It accepts HMAC-SHA256-signed deliveries, deduplicates by
delivery id, records a committed delivery lease before invoking a sink, and
allows a fresh worker to reclaim a lease after the original worker dies.

The prior cases establish the progression:

- Test Ledger: CLI, JSON-file persistence, validation, and fresh processes;
- Read Queue: HTTP, SQLite, CRUD validation, and server restart recovery;
- Event Outbox: idempotent admission, retry state, and a direct-argv NDJSON
  sink worker.

Webhook Inbox adds authentication and a recoverable leased state machine while
remaining a single standard-library project. The specification is in
`project/SPEC.md`; the independent black-box oracle is
`acceptance/test_webhook_inbox.py`.

## Branch, PR, and baseline

The branch was created from a clean checkout exactly aligned with
`origin/main`. The first commit was pushed before any implementation work:

```text
git switch -c tester/2026-09-20-loop-3-webhook-inbox origin/main
git commit -m "test: define loop 3 Webhook Inbox live case"
e4a4ceb
git push -u origin tester/2026-09-20-loop-3-webhook-inbox
gh pr create --draft --base main --head tester/2026-09-20-loop-3-webhook-inbox
```

GitHub accepted draft PR #111. No merge was performed. The initial commit
contains only the plan, specification, independent oracle and sinks, three
configs, and ignore rules.

Before project implementation, the exact baseline command was:

```text
python -m unittest discover -s docs/cases/2026-09-20-webhook-inbox/acceptance -p "test_*.py" -v
```

It exited `1` after `16.688s`; both tests failed during setup because the
fresh server process could not import the missing `webhookinbox` package and
the health probe timed out with Windows connection-refused errors. This was
the expected missing-project baseline.

## Live rupi runs

The current `target/debug/rupi.exe` was rebuilt from the branch and exited 0.
The endpoint probe returned HTTP 200 and listed `qwen3.8-flash-next`.

### R-01 — Initial bounded implementation turn

Config: `project/rupi.config.json` (`thinking: low`, 16 model-request limit,
16,384 output-token capability cap). The command was run from `project/`:

```text
rupi.exe run --config rupi.config.json --cwd . --prompt <implementation prompt> --no-color --no-reasoning --verbose
```

The prompt required the complete implementation from `SPEC.md`, standard
library only, direct `python.exe` argv on Windows, an early project suite,
three help checks, no oracle access, and exact results.

Exact prompt:

```text
Read SPEC.md and build the complete Webhook Inbox project in this workspace.

This is one bounded implementation slice. Work only under the current project workspace; do not edit SPEC.md, any rupi config, or files outside this workspace. Do not inspect or run the external acceptance oracle. Use only Python standard-library modules. Create the webhookinbox package, a readable README.md, and focused tests. Implement the exact HMAC-signed HTTP contract, deterministic SQLite persistence, idempotent/conflicting delivery admission, committed delivery leases, fresh-worker reclaim after lease expiry, and the direct-argv newline-delimited JSON sink protocol from SPEC.md.

On this Windows host, use the direct process tool for known programs such as python.exe with an argv list. Do not spend requests on Unix ls, shell wrappers, or fragile python -c quoting. Use write/edit for files. Run the project unittest command after the core implementation exists and fix only project defects required by SPEC.md:
python.exe -W error::ResourceWarning -m unittest discover -s tests -p test_*.py -v
Then run the top-level, serve, and worker help commands with direct process argv. Report each exact exit status and observed test summary. Do not claim completion from planned commands. Stop after the bounded project is implemented, tested, and documented.
```

Session: `01a0c006-8f61-76ee-baec-36fc858fa6b6`.

Observed trace facts:

- 8 model requests started; 7 completed before interruption;
- 7 completed request tool-call boundaries, 8 tool requests, 7 successful
  tool completions, and 1 failed tool call;
- 14,216 trace entries were durable;
- trace wall span: `782,341 ms` (about 13m 02s);
- no project files were written;
- the terminal was interrupted with Ctrl-C while a model request was still
  generating, so there was no clean `turn_completed` or `session_ended` event.

The first avoidable Windows command was:

```text
exec command=dir /b "%CD%" && echo --- && cd
```

It failed with:

```text
The filename, directory name, or volume label syntax is incorrect.
```

The model then used `dir /b` successfully, read all three configs, and ran
`python.exe -V`, but never reached a write.

### R-02 — Focused recovery attempt

Config: `project/rupi.recovery.config.json` (`thinking: low`, 8 model-request
limit, 8,192 output-token capability cap). The recovery prompt explicitly said
that no files existed, instructed the model to write the package immediately,
forbade directory/config/oracle inspection, and limited verification to the
project suite and three help commands.

Exact prompt:

```text
This is a focused recovery slice after an interrupted turn. No project files were created. Immediately write the minimal complete Webhook Inbox implementation from SPEC.md: create webhookinbox/, README.md, and focused tests. Work only in this workspace; do not read configs, inspect directories, or access the external acceptance oracle. Use write/edit for source files. Use only Python standard library. Implement the HMAC signature check, exact HTTP validation/idempotency contract, SQLite delivery records, lease claim/expiry/reclaim, direct-argv NDJSON sink worker, and the documented help commands. Do not make probe files or broad abstractions.

After the implementation exists, run exactly this project suite once using the direct process tool with program python.exe and argv, not a shell:
python.exe -W error::ResourceWarning -m unittest discover -s tests -p test_*.py -v
If it fails, fix only project defects required by SPEC.md and rerun once. Then run python.exe -m webhookinbox --help, python.exe -m webhookinbox serve --help, and python.exe -m webhookinbox worker --help with direct argv. Report exact statuses and stop. Do not claim success from planned commands.
```

Command shape:

```text
rupi.exe run --config rupi.recovery.config.json --cwd . --prompt <recovery prompt> --no-color --no-reasoning --verbose
```

Session: `01a0c013-22c2-7884-b615-e724d66ae7b7`.

Observed trace facts:

- 3 model requests started; 2 completed;
- both completed requests only read `SPEC.md`;
- 5,389 trace entries were durable;
- trace wall span: `273,150 ms` (about 4m 33s);
- no project files were written;
- the recovery process was interrupted with Ctrl-C while the next request was
  still generating.

The committed project was therefore not model-created. This is retained as a
runtime/UX finding rather than represented as successful implementation.

## Bounded tester repair

The case plan permits a project-local repair after an incomplete live turn.
The tester added only files under this case directory:

- `project/webhookinbox/`: typed validation, deterministic SQLite store,
  signed HTTP adapter, explicit lease transitions, and direct-argv worker;
- `project/tests/`: 11 focused tests covering pure validation, idempotency,
  lease expiry/reclaim, terminal delivery, sink failure boundedness, HTTP
  errors, and CLI help;
- `project/README.md`: exact commands, signature construction, routes,
  lease/reclaim behavior, sink protocol, persistence, and checks.

The repair does not modify `src/`, `crates/`, repository `tests/`, or any
canonical document. It is acceptance evidence for T, not evidence that R-01 or
R-02 completed the project.

## Independent project and oracle evidence

Project suite, run from `project/`:

```text
python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v
Ran 11 tests in 1.610s
OK
exit=0
```

Independent oracle, run from the repository root:

```text
python -W error::ResourceWarning -m unittest discover -s docs/cases/2026-09-20-webhook-inbox/acceptance -p "test_*.py" -v
Ran 2 tests in 3.601s
OK
exit=0
```

The oracle imports no project modules. It started the service, checked health
and unknown ids, admitted a signed delivery, checked exact duplicate and
conflicting duplicate behavior, rejected a bad signature and malformed signed
JSON, delivered through a fresh direct-argv sink, killed a fresh worker after
the lease commit, observed the active lease, waited for expiry, reclaimed the
delivery with another worker, and restarted the server against the same SQLite
file. The reclaimed delivery had `attempts == 2`; delivered rows remained
delivered after restart. The sink also received the option-like `literal;data`
value as an argv value rather than shell syntax.

The project and oracle import only Python standard-library modules. Top-level,
`serve`, and `worker` help commands each exited 0 independently.

## Read-only rupi verification

The final bounded verification prompt forbade writes and oracle access and ran
the project suite plus the three help commands through direct `python.exe`
argv. Session: `01a0c01d-aae3-70f3-90bc-6d4b1f4a9d6a`.

```text
model requests: 2
direct process calls: 4
project tests: Ran 11 tests in 1.495s; OK
help checks: 3/3 exit 0
turn status: completed
rupi exit: 0
elapsed: 55.960s
```

The verification trace had 704 entries, 4 process lifecycles, and a clean
`turn_completed`/`session_ended` pair. It did not write files.

## Trace and replay

All material sessions remained inspectable after interruption or completion:

```text
rupi.exe trace --config rupi.config.json 01a0c006-8f61-76ee-baec-36fc858fa6b6 --quiet --no-reasoning --no-color
exit 0; 14,216 entries read; failed session state displayed

rupi.exe replay .rupi-state/sessions/01a0c006-8f61-76ee-baec-36fc858fa6b6.trace.jsonl --tools --sequence
exit 0; replayed reads, the failed shell exec, and the successful process probe

rupi.exe trace --config rupi.recovery.config.json 01a0c013-22c2-7884-b615-e724d66ae7b7 --quiet --no-reasoning --no-color
exit 0; 5,389 entries read

rupi.exe replay .rupi-state-recovery/sessions/01a0c013-22c2-7884-b615-e724d66ae7b7.trace.jsonl --tools --sequence
exit 0; replayed the two SPEC reads

rupi.exe trace --config rupi.verify.config.json 01a0c01d-aae3-70f3-90bc-6d4b1f4a9d6a --quiet --no-reasoning --no-color
exit 0; 704 entries read

rupi.exe replay .rupi-state-verify/sessions/01a0c01d-aae3-70f3-90bc-6d4b1f4a9d6a.trace.jsonl --tools --sequence
exit 0; replayed four successful process lifecycles
```

Replay was read-only in all cases; no historical tool or sink was executed.

## Findings

### F-01 — High: local implementation turns can make no progress for minutes

Both implementation attempts reached multiple model requests and durable trace
state without a single project write. R-01 spent about 13 minutes and R-02
about 4.5 minutes before interruption. The model reread the specification and
performed low-value environment inspection despite prompts that explicitly
required immediate writes. The project could be completed only by a bounded
tester repair.

This is the same major rupi stopgate observed by the earlier cases, now
reproduced on a third, incrementally harder project. The independent acceptance
result must not be read as evidence that the live implementation turns
completed.

### F-02 — Medium: shell guidance remains imperfect on Windows

R-01 issued a shell-shaped `dir /b "%CD%" && ...` command despite direct-argv
guidance, producing a command syntax failure. A later `dir /b` succeeded, but
the avoidable failure consumed a model request and reinforced the prior cases'
Windows friction finding.

### O-01 — Positive: durable inspection survives interrupted turns

Both interrupted implementation sessions produced readable traces and replay
projections. The final verification session also replayed four direct process
lifecycle pairs without contacting the model or rerunning historical commands.

### O-02 — Positive: the runtime verification boundary was usable

Once the project existed, a fresh rupi process ran the exact project suite and
help checks through direct argv, reported exact statuses, and stopped without
editing the workspace.

## Major stopgate and precise retry request

The case itself is complete after project-local repair, but a major rupi issue
remains: a fresh local Qwen implementation turn has no reliable first-write or
per-request progress bound. The smallest reproduction is R-02: live endpoint
HTTP 200; `rupi.recovery.config.json` with 8 request max and 8,192 output cap;
3 requests started, 2 completed; both completed requests only read `SPEC.md`;
273,150 ms trace span; Ctrl-C; zero project files.

Parent retry request: add or use a supported per-request deadline/progress
boundary that prevents an implementation turn from spending several minutes
in opaque generation before its first write, then rerun this exact branch case
with the same `SPEC.md`, initial prompt, and recovery prompt. Compare (1) time
to first project write, (2) requests/tool calls before that write, (3) whether
the model completes the project suite and README without tester repair, and
(4) whether the interrupted trace still replays. Do not change the case oracle
or pre-create project implementation files to make that retry pass.

## Parent-fix retry (2026-09-20)

The retry requested after parent commit `96ca4b3` is recorded in
[`RETRY_REPORT.md`](RETRY_REPORT.md). It used a fresh ignored workspace with
only copied specs/configs/ignore rules and an unchanged copied oracle; the
committed tester-repaired project was not used as implementation output.

All three configs parsed successfully before running. The initial retry used
session `01a0c02b-9f19-74d4-8d96-5e57b53e0297`: 82,234 ms, 5 model requests
started/completed, 7 tool requests, 6 completions, 1 failure, and no project
write. It exited `1` with typed `turn_completed` timeout. The focused recovery
used session `01a0c02d-836d-749b-ab4f-17801a8dd9a9`: 51,713 ms, 3 model
requests started/completed, 2 tool requests, 2 completions, 0 failures, and no
project write; it also exited `1` with typed timeout.

Neither retry produced `webhookinbox/`, `README.md`, or `tests/`, so the
model-authored project suite and unchanged independent oracle were not run.
Running them against the prior tester repair would have invalidated the retry
measurement. Both final trace reads and tool replays exited `0` (788 and 237
trace entries respectively). The high stopgate remains: wall-time exposure was
reduced, but the first-write/implementation failure was not resolved.

## Final progress-boundary retry (2026-09-20)

The parent runtime fix `e2e00f3` was built before this retry. Detailed evidence
is in [`PROGRESS_RETRY_REPORT.md`](PROGRESS_RETRY_REPORT.md). A brand-new
ignored workspace contained only copied specs/configs/ignore rules plus an
unchanged oracle; no prior retry state or tester implementation was visible in
the model cwd.

All configs parsed successfully. The initial session
`01a0c03c-0168-7e3d-901b-4bff5ceac0dd` ran 37,121 ms with 2/2 model request
starts/completions and 2/2/0 tool request/completion/failure counts. The
recovery session `01a0c03c-d318-7228-a8af-eace6d50c489` ran 38,077 ms with the
same counts. Both made no project write and exited `1` with typed timeout.

The boundary evidence is positive but incomplete: each trace exposed 7 normal
tool schemas, appended the exact runtime nudge and info diagnostic after one
no-progress request, then exposed 3 schemas (`write`, `edit`, `append`). Neither
model requested a progress tool before timing out. No model package, README,
tests, project suite, or oracle result exists. Trace/replay exited `0` for both
sessions (214 and 473 entries). The high stopgate remains, operationally
reduced by the boundary but not resolved.

## Final provider-effort retry (2026-09-20)

The final retry after `fc56ac4` is recorded in
[`REASONING_RETRY_REPORT.md`](REASONING_RETRY_REPORT.md). Rupi was rebuilt from
that commit, and a brand-new ignored workspace contained only copied
spec/config/ignore inputs plus the unchanged oracle. The initial and recovery
configs parsed successfully with explicit `thinking: low`, 60-second request
timeouts, bounded request counts, and the unchanged progress boundary.

R-07 initial session `01a0c042-e535-7ea5-87ca-0912a63b76ea` ran 66,666 ms with
2/2 model request starts/completions and 2/2/0 tool request/completion/failure
counts. R-08 recovery session `01a0c044-2e31-7a68-a005-c18ca45f33cd` ran 70,365
ms with 2/2 requests and 1/1/0 tools. Both made no project write and exited
`1` with typed timeout.

Both traces show the boundary transition from 7 to 3 schemas, the exact
runtime nudge and info diagnostic, and no `write`, `edit`, or `append` call.
Neither model produced project files, so the project suite and unchanged oracle
were not run. Trace/replay exited `0` (892 and 1,162 entries). Explicit low
provider effort did not resolve the high stopgate; it remains.

## Final environment-only retry (`--reasoning off`, 2026-09-20)

The final environment retry is recorded in
[`REASONING_OFF_RETRY_REPORT.md`](REASONING_OFF_RETRY_REPORT.md). The local
llama server was restarted with the user-authorized isolated `--reasoning off`
setting; `/v1/models` was healthy for `qwen3.8-flash-next`. Rupi was rebuilt
from `20a311f`, and a brand-new ignored workspace contained only copied
spec/config/ignore inputs plus the unchanged oracle.

The initial session `01a0c04a-d453-733d-9581-280c626a7a47` ran 117,161 ms with
5/5 model request starts/completions, 4/4/0 tool request/completion/failure
counts, and a first model write at 40,237 ms (`webhookinbox/__init__.py`). The
recovery session `01a0c04d-093f-71e2-a55c-60a6a76f699d` ran 67,192 ms with 3/3
requests and 2/2/0 tools, adding no files. Both ended with typed provider
quarantine after a 60-second timeout/retry sequence.

The boundary evidence was durable: initial request schemas narrowed 7 -> 3
(`write`, `edit`, `append`) twice; recovery narrowed 7 -> 3 once. The model
authored only `webhookinbox/__init__.py`; the project suite failed because
`tests/` was absent, and the unchanged oracle ran against the partial workspace
and failed both tests at `/healthz` setup. Trace/replay exited `0` (63 and 37
entries). The high stopgate remains, materially reduced by the first write but
not resolved.

## Final one-shot progress-boundary retry (`8efea83`, 2026-09-20)

The final retry after parent fix `8efea83` is recorded in
[`ONE_SHOT_RETRY_REPORT.md`](ONE_SHOT_RETRY_REPORT.md). Rupi was rebuilt from
the current head, and a brand-new ignored workspace contained only copied
`SPEC.md`, configs, `.gitignore`, and the unchanged oracle/sinks. No prior
implementation or retry files were exposed, and no implementation file was
pre-created or deleted.

The local llama server was healthy in user-authorized `--reasoning off` mode.
All configs passed the pre-run parse check (the expected `parse-probe` session
lookup returned no sessions, with no invalid-config error). The initial session
`01a0c056-6029-729d-a93b-af8b2d7e09ac` ran 107,129 ms with 5/5 model request
starts/completions, one model retry, and 5/5/0 tool request/completion/failure
counts. Its first project write was `webhookinbox/__init__.py` at 30,149 ms;
it then wrote `package_info.py` before the 60-second timeout and typed provider
quarantine. The recovery session
`01a0c058-4a7e-7bc7-8605-0fb1195a0f74` ran 68,570 ms with 3/3 requests and
3/3/0 tools; it first wrote `__init__.py` at 25,833 ms and then `__main__.py`,
ending naturally at `budget_exhausted`.

The one-shot boundary behaved as intended. In the initial trace, schemas were
7 at request 4, narrowed to `write`, `edit`, `append` at request 28 after
one nudge/info diagnostic, and returned to all 7 at request 36 after successful
writes; no second boundary nudge/diagnostic appeared. Recovery showed the same
7 -> 3 transition before its two writes. The model authored only
`webhookinbox/__init__.py`, `package_info.py`, and `__main__.py`; no README,
tests, or functional modules existed. The unchanged project suite failed with
`ImportError: Start directory is not importable: 'tests'`; the unchanged oracle
ran because model output existed and both tests failed at `/healthz` with
connection refused. Trace/replay exited 0 for both sessions (49/542 entries;
15/9 tool lifecycle frames shown/replayed).

The high stopgate remains, materially reduced: the runtime boundary now allows
normal schemas after successful progress and the model reaches first writes,
but it still does not complete T or pass either acceptance check without tester
repair.

## Final 120-second budget retry (`5ab2dc9`, 2026-09-20)

The final implementation retry is recorded in
[`BUDGET_RETRY_REPORT.md`](BUDGET_RETRY_REPORT.md). Rupi was rebuilt from
`5ab2dc9`. The user-authorized llama server remained healthy in
`--reasoning off` mode. A brand-new ignored workspace contained only the
committed `SPEC.md`, configs, `.gitignore`, and unchanged oracle/sinks; no
prior retry or implementation files were copied, created, or deleted.

All three config probes returned the expected no-session result with no invalid
config error. R-13 initial session
`01a0c065-0a45-74db-89ad-efbfc70a510f` ran 200,325 ms, with 5/5 model request
starts/completions and one retry, 5/5/0 tool request/completion/failure counts,
and a first write at 51,272 ms. It wrote `__init__.py`, `signature.py`, and
`schema.py`, then ended in typed provider quarantine after the 120-second
request timeout.

R-14 recovery session `01a0c068-670a-795d-a220-ed5f8452f1d4` ran 215,824 ms,
with 6/6/0 model requests/retries and 7/6/1 tool request/completion/failure
counts. It first wrote `README.md` at 76,102 ms, then wrote `__init__.py` and
`signing.py`, ending naturally at `budget_exhausted`. One attempted read of an
outside-workspace skill file failed and was retained in the trace.

The one-shot boundary remained correct: initial schemas changed 7 -> 3 -> 7
after successful writes, with one nudge/info diagnostic and no second
activation; recovery showed the same 7 -> 3 -> 7 transition before its final
0-tool request. The model-authored workspace had no server, store, worker, CLI,
or tests. The unchanged project suite failed because `tests/` was absent, and
the unchanged oracle ran because model output existed but both tests failed at
`/healthz` with connection refused. Trace/replay exited 0 for both sessions
(75/1,261 entries; 15/20 tool lifecycle frames).

The high stopgate remains. The longer deadline and recovery budget increased
partial output, but did not produce a complete project or passing acceptance.
The initial prompt was verbatim. R-14 accidentally appended one sentence not
present in the committed recovery prompt: `Treat the task as incomplete if any
required file or verification is unfinished.` This prompt-fidelity deviation
is recorded in the retry report; no further Webhook Inbox retry was performed
because this was the final implementation retry.
