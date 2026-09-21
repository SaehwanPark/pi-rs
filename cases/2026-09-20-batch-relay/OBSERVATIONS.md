# Batch Relay live-case observations

Date: 2026-09-20
Operator: Codex acting as a fresh rupi user
Host: Windows PowerShell, Python 3.14.7, AMD Ryzen AI Max+ 395
Branch: tester/2026-09-20-loop-4-batch-relay
Base: clean origin/main at e36c60e
Draft PR: https://github.com/SaehwanPark/rupi/pull/112
Endpoint: http://127.0.0.1:8000/v1, model qwen3.8-flash-next

## Selected project

T is Batch Relay, a dependency-free Python HTTP/SQLite service plus a separate
worker. It accepts HMAC-SHA256-signed batches, validates and stores an acyclic
dependency DAG atomically, and lets a direct-argv sink process runnable jobs.
The worker records retryable and terminal outcomes, blocks dependents of failed
jobs, commits leases before invoking the sink, and lets a fresh worker reclaim
an expired lease.

The prior ladder was:

- Test Ledger: CLI, JSON persistence, validation, and fresh processes.
- Read Queue: HTTP, SQLite, CRUD validation, and restart recovery.
- Event Outbox: idempotent admission, retry state, and a direct-argv worker.
- Webhook Inbox: HMAC admission, leases, crash reclaim, and restart recovery.

The specification is project/SPEC.md. The independent black-box oracle is
acceptance/test_batchrelay.py; it does not import batchrelay and starts the
server, worker, and fake sink as separate processes.

## Branch and draft PR setup

The repository was clean and main matched origin/main before the branch:

~~~~text
git fetch origin main
git switch -c tester/2026-09-20-loop-4-batch-relay origin/main
git add docs/cases/2026-09-20-batch-relay
git commit -m "test: define Loop 4 Batch Relay live case"
1855486
git push -u origin HEAD
gh pr create --repo SaehwanPark/rupi --base main --head tester/2026-09-20-loop-4-batch-relay --draft --title "test: live case 4 Batch Relay"
~~~~

GitHub created draft PR #112 before implementation work. No merge was
performed. Only docs/cases/2026-09-20-batch-relay/ is in scope.

## Endpoint and binary readiness

Exact endpoint check:

~~~~powershell
$sw=[Diagnostics.Stopwatch]::StartNew(); try { $r=Invoke-WebRequest -UseBasicParsing -Uri 'http://127.0.0.1:8000/v1/models' -TimeoutSec 10; $code=[int]$r.StatusCode; $body=$r.Content } catch { $code=0; $body=$_.Exception.Message }; $sw.Stop(); "endpoint_status=$code elapsed_ms=$($sw.ElapsedMilliseconds)"; $body
~~~~

Observed: HTTP 200, 73 ms; the response listed qwen3.8-flash-next,
owned by llamacpp, with context size 262144.

Exact build command:

~~~~powershell
cargo build --bin rupi
~~~~

Observed: exit 0, 6.38 s.

## Independent baseline before implementation

The committed oracle was run before the project package existed:

~~~~powershell
$sw=[Diagnostics.Stopwatch]::StartNew(); python -W error::ResourceWarning -m unittest discover -s acceptance -p "test_*.py" -v; $code=$LASTEXITCODE; $sw.Stop(); Write-Output ("oracle_baseline_clean_exit={0} elapsed_ms={1}" -f $code,$sw.ElapsedMilliseconds); exit 0
~~~~

Observed: Python exit 1, wrapper-reported 33,394 ms, Ran 4 tests, and all
four failed in setUp because the fresh server could not import the missing
batchrelay package and /healthz was connection-refused.

An earlier baseline attempt before oracle cleanup took 33,382 ms and exposed
ResourceWarnings for unclosed failed-start subprocess pipes and temporary
directories. The tester corrected only that independent-oracle cleanup before
the baseline commit.

## Initial live implementation turn

The project directory contained only .gitignore, SPEC.md, the three rupi
configs, and no implementation package or tests.

Exact command shape:

~~~~powershell
rupi.exe run --config rupi.config.json --cwd . --prompt $prompt --no-color --no-reasoning --verbose
~~~~

The exact prompt was:

~~~~text
Read SPEC.md and build the complete Batch Relay project in this workspace.

This is one bounded implementation slice. Work only under the current project workspace; do not edit SPEC.md, any rupi config, or files outside this workspace. Do not inspect or run the external acceptance oracle. Use only Python standard-library modules. Create the batchrelay package, a readable README.md, and focused tests. Implement the exact HMAC-authenticated HTTP contract, atomic dependency-DAG batch admission, deterministic SQLite state transitions, runnable-job ordering, retryable and terminal sink failures, blocked dependents, lease expiry/reclaim, and the direct-argv newline-delimited JSON sink protocol from SPEC.md.

On this Windows host, use the direct process tool for known programs such as python.exe with an argv list. Do not spend requests on Unix ls, shell wrappers, or fragile python -c quoting. Use write/edit for files. Run the project unittest command after the core implementation exists and fix only project defects required by SPEC.md:
python.exe -W error::ResourceWarning -m unittest discover -s tests -p test_*.py -v
Then run the top-level, serve, and worker help commands with direct process argv. Report each exact exit status and observed test summary. Do not claim completion from planned commands. Stop after the bounded project is implemented, tested, and documented.
~~~~

Observed:

- rupi session: 01a0c087-2aad-7a01-9b5b-2bcbce45e86f.
- Wrapper elapsed: 138,062 ms; process exit 1.
- Trace: .rupi-state/sessions/01a0c087-2aad-7a01-9b5b-2bcbce45e86f.trace.jsonl,
  1,189 entries, 522,423 bytes.
- Two model requests started/completed. Durations were 15,576 ms and
  122,056 ms; the second request ended at the 120,000 ms deadline.
- Two tool calls completed: read SPEC.md, then exec dir /b.
- The model made no write/edit/append call and no implementation file existed
  after the turn.
- The progress-boundary diagnostic activated after one request without a
  configured progress tool and exposed write, edit, and append on the next
  request.
- Final status was turn_completed.status.failed.kind=timeout followed by an
  interrupted session end. No retry or failover event occurred.

The model used dir /b despite the direct-argv instruction. It succeeded, but
consumed a tool-bearing request and reproduced the earlier Windows friction.

Independent empty-project check:

~~~~powershell
$sw=[Diagnostics.Stopwatch]::StartNew(); python.exe -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v; $code=$LASTEXITCODE; $sw.Stop(); "project_baseline_exit=$code elapsed_ms=$($sw.ElapsedMilliseconds)"; exit 0
~~~~

Observed: Python exit 1, 105 ms, ImportError: Start directory is not
importable: 'tests'.

Read-only trace:

~~~~powershell
rupi.exe trace --config rupi.config.json --session 01a0c087-2aad-7a01-9b5b-2bcbce45e86f --tools --sequence --no-color --no-reasoning
~~~~

Exit 0, 1,189 entries read, 6 shown, 30 ms.

Read-only replay:

~~~~powershell
rupi.exe replay .rupi-state\sessions\01a0c087-2aad-7a01-9b5b-2bcbce45e86f.trace.jsonl --tools --sequence
~~~~

Exit 0, 36 ms. It replayed only the historical read/exec lifecycle and did
not contact the provider or execute the old command.

## Recovery turn

The recovery configuration used six model requests, an 8,192 output cap, a
separate state directory, and the same 120,000 ms request timeout.

Exact command shape:

~~~~powershell
rupi.exe run --config rupi.recovery.config.json --cwd . --prompt $prompt --no-color --no-reasoning --verbose
~~~~

The exact recovery prompt was:

~~~~text
This is a focused recovery slice after a timed-out implementation turn. The project workspace still contains only SPEC.md, the three rupi configs, and .gitignore; no implementation files exist. Immediately build the minimal complete Batch Relay project from SPEC.md: create batchrelay/, README.md, and focused tests. Work only in this workspace; do not inspect configs or directories, do not access the external acceptance oracle, and do not make probe files. Use only Python standard-library modules. Implement the exact signed HTTP contract, atomic dependency-DAG validation/admission, SQLite state, dependency-aware worker scheduling, retryable and terminal sink outcomes, blocked dependents, lease reclaim, direct-argv NDJSON protocol, and the documented help commands.

On Windows use write/edit for files and the direct process tool with program python.exe and argv for commands, not shell syntax. After the implementation exists, run exactly this project suite once:
python.exe -W error::ResourceWarning -m unittest discover -s tests -p test_*.py -v
If it fails, fix only defects required by SPEC.md and rerun once. Then run python.exe -m batchrelay --help, python.exe -m batchrelay serve --help, and python.exe -m batchrelay worker --help with direct argv. Report exact statuses and stop. Do not claim success from planned commands.
~~~~

Observed:

- rupi session: 01a0c089-f064-7aa5-8ccb-21bfc0a7966c.
- Wrapper elapsed: 127,524 ms; process exit 1.
- Trace: .rupi-state-recovery/sessions/01a0c089-f064-7aa5-8ccb-21bfc0a7966c.trace.jsonl,
  2,411 entries, 1,057,219 bytes.
- Two model requests started/completed. Durations were 7,374 ms and 120,044 ms.
- One tool call completed: read SPEC.md; no project write occurred.
- The progress-boundary diagnostic activated again; final status was typed
  timeout, with no retry/failover and no model-authored project files.

The project remained at its five baseline files. This was the second bounded
implementation attempt and the case-level model-authoring stopgate is active.
No additional implementation retry was run.

Recovery trace:

~~~~powershell
rupi.exe trace --config rupi.recovery.config.json --session 01a0c089-f064-7aa5-8ccb-21bfc0a7966c --tools --sequence --no-color --no-reasoning
~~~~

Exit 0, 2,411 entries read, 3 shown, 44 ms.

Recovery replay:

~~~~powershell
rupi.exe replay .rupi-state-recovery\sessions\01a0c089-f064-7aa5-8ccb-21bfc0a7966c.trace.jsonl --tools --sequence
~~~~

Exit 0, 60 ms; only the historical read lifecycle was replayed.

## Bounded tester repair

Because both model turns timed out before the first project write, the tester
implemented the already-committed specification under the project directory.
No rupi source, Rust crate, repository test, canonical document, or roadmap file
was changed. The repair added:

- project/batchrelay/: CLI, HMAC HTTP server, SQLite store, DAG validation,
  explicit job states, direct-argv worker, lease expiry/reclaim, retryable and
  terminal sink handling, and blocked dependents;
- project/tests/: 9 focused tests;
- project/README.md: commands, signing, routes, state transitions, leases,
  sink protocol, persistence, and checks.

The first post-repair oracle run exposed one scoped contract issue: a retryable
job was retried immediately in the same --once invocation, making retry state
unobservable. The tester clarified the case contract and implementation so one
--once invocation attempts each job at most once and defers retryable jobs to a
later invocation. The oracle also corrected a log assertion and closed
subprocess/HTTP-error handles. These were project/oracle corrections, not rupi
runtime changes.

Project suite rerun:

~~~~powershell
$sw=[Diagnostics.Stopwatch]::StartNew(); python.exe -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v; $code=$LASTEXITCODE; $sw.Stop(); "project_repair_suite_rerun_exit=$code elapsed_ms=$($sw.ElapsedMilliseconds)"; exit $code
~~~~

Observed: Ran 9 tests in 1.678s, OK, Python exit 0, wrapper elapsed 1,813 ms.

## Independent acceptance after repair

Final oracle command:

~~~~powershell
$sw=[Diagnostics.Stopwatch]::StartNew(); python.exe -W error::ResourceWarning -m unittest discover -s acceptance -p "test_*.py" -v; $code=$LASTEXITCODE; $sw.Stop(); Write-Output ("oracle_repair_final_exit={0} elapsed_ms={1}" -f $code,$sw.ElapsedMilliseconds); exit $code
~~~~

Observed: Ran 4 tests in 5.515s, OK, Python exit 0, wrapper elapsed 5,640 ms.

The four oracle tests covered invalid signature and cyclic DAG rejection,
atomic admission, idempotent/conflicting batches, dependency-order delivery,
retry deferral, terminal failure, blocked dependents, lease crash/reclaim,
restart persistence, direct argv, and all three help commands. The oracle uses
only standard-library imports and starts the project processes independently.

## Read-only rupi verification

Exact command shape:

~~~~powershell
rupi.exe run --config rupi.verify.config.json --cwd . --prompt $prompt --no-color --no-reasoning --verbose
~~~~

Exact prompt:

~~~~text
Perform a bounded read-only verification of the completed Batch Relay project. Do not edit, write, or delete any file and do not inspect or run the external acceptance oracle. Use the direct process tool with program python.exe and an argv list, not shell syntax. Run exactly these four checks from the current workspace:
1) python.exe -W error::ResourceWarning -m unittest discover -s tests -p test_*.py -v
2) python.exe -m batchrelay --help
3) python.exe -m batchrelay serve --help
4) python.exe -m batchrelay worker --help
Report each exact exit status and the observed test summary. Stop after these bounded checks; do not claim any unrun check.
~~~~

Observed:

- rupi session: 01a0c092-0931-75da-9bb9-b5b751e343bc.
- Wrapper elapsed: 50,658 ms; process exit 0.
- Two model requests started/completed; the first made exactly four successful
  process calls and the second supplied the final answer.
- Project suite: Ran 9 tests in 1.714s, OK, exit 0.
- Top-level help, serve help, and worker help each exited 0.
- No write/edit/append call and no acceptance-oracle access occurred.

Read-only trace:

~~~~powershell
rupi.exe trace --config rupi.verify.config.json --session 01a0c092-0931-75da-9bb9-b5b751e343bc --quiet --no-reasoning --no-color
~~~~

Exit 0, 688 entries read, 0 shown because quiet filters routine events.

Read-only replay:

~~~~powershell
rupi.exe replay .rupi-state-verify\sessions\01a0c092-0931-75da-9bb9-b5b751e343bc.trace.jsonl --tools --sequence
~~~~

Exit 0, 28 ms; it replayed four historical process lifecycles and did not
execute them again.

The live traces record native reasoning provenance, one local active model, no
failover, and no uncertain mutating tool outcome. The implementation runs are
incomplete timeout evidence; the four acceptance checks, not model prose, are
the project result.

## Repository validation

~~~~text
cargo fmt --all --check                         exit 0
cargo check -p rupi-core --all-features         exit 0
cargo clippy --workspace --all-targets -- -D warnings
                                                exit 0
cargo test --workspace                         exit 0, all workspace tests passed
cargo doc --workspace --no-deps                 exit 0
~~~~

The first direct bash bench/startup.sh --json bench/results/startup-ci.json
attempt exited 1 because the default WSL Bash environment could not resolve
PowerShell's Windows cargo.exe. The successful environment-only rerun used
Git Bash explicitly:

~~~~powershell
& 'C:\Program Files\Git\bin\bash.exe' -c 'export PATH="/c/Users/saehwan/.cargo/bin:$PATH"; command -v cargo; bash bench/startup.sh --json bench/results/startup-ci.json'
~~~~

It exited 0 and recorded:

~~~~text
Cold startup: 140.47 ms
Warm startup (10 runs): min 5.82 ms, mean 6.30 ms, median 6.18 ms, max 6.72 ms
~~~~

The benchmark wrote bench/results/startup-ci.json; it is not part of this
case diff.

## Findings and stopgates

### F-01 — High: repeated local-model first-write stopgate

Both bounded implementation turns timed out after one specification read and
made no project write. The initial run used two requests and 138,062 ms; the
recovery used two requests and 127,524 ms. The progress boundary activated in
both traces, but the model did not reach a write call before the provider
deadline. This is not a claim that Qwen could never complete T; it is the
reproducible bounded result on this fresh workspace and configuration.

### F-02 — Medium: Windows shell guidance was not followed consistently

The initial model used exec dir /b despite the direct-argv instruction. The
command succeeded, but the avoidable environment probe consumed a tool-bearing
request. This is consistent with prior cases and remains first-task UX friction.

### F-03 — Low: oracle boundary needed one tester correction

The first post-repair oracle run found same-invocation retry semantics, a log
assertion mistake, and resource cleanup gaps. The tester corrected the case
contract/oracle and reran independently. They are not rupi defects.

### O-01 — Positive: trace/replay remained durable and read-only

Both timed-out implementation sessions and the completed verification session
were readable through rupi trace and rupi replay. Replay produced lifecycle
projections only; it did not contact Qwen, rerun Python, or mutate the project.

### O-02 — Positive: explicit provenance and single-model execution were preserved

The traces attribute native reasoning to local/qwen3.8-flash-next; no hidden
reasoning was reconstructed, no backup model was activated, and no mutating
operation reached an uncertain completion state in this case.

There is no environment or acceptance stopgate remaining. The model-authoring
stopgate remains active: T passed only after tester repair, and the two model
turns must remain classified as incomplete.
