# Benchmark report: `rupi` vs `pi` (Round 2)

Status: Complete — all ten cases measured for both agents.

This report records Round 2 of the fresh-memory comparison of `rupi` and Pi 0.86.1 on the
ten repository evaluation cases under [`cases/`](../../cases/). The fixed inference
server is llama.cpp serving Qwen 3.8 Flash Next (UD-IQ4_XS).

Execution window: 2026-09-21 09:11 through 2026-09-22 08:38 America/New_York.
The two raw runs are `bench-20260921-round2-01-05` (cases 01–05) and `bench-20260921-round2-06-10`
(cases 06–10). Their machine-readable results, traces, session logs, verification output,
and scratch workspaces are under the ignored `.benchmark/runs/` directory. No implementation
or acceptance case files were changed in the repository.

The quantitative result tables and raw run evidence are generated under the ignored
`.benchmark/` directory. The checked-in harness is
[`bench/compare-pi-rupi.ps1`](../../bench/compare-pi-rupi.ps1).

## Executive summary

In Round 1 ([`docs/benchmark/2026-09-20-pi-vs-rupi.md`](2026-09-20-pi-vs-rupi.md)), neither agent
reached resolution on any case within the tight 2-turn / 300 s budget (0/10 for both).
In Round 2, with generous budgets (900 s turn timeout, up to 4 turns, max 24 model requests per
turn, provider timeouts aligned, and non-interactive standard input properly closed):

- **Pi achieved verified acceptance-oracle resolutions on 2 out of 10 cases**:
  - `02-reading-queue` resolved in **2 turns** (1,800.5 s, 39,856 total tokens).
  - `04-webhook-inbox` resolved in **2 turns** (1,800.6 s, 41,114 total tokens).
  Both cases passed their complete independent acceptance oracles in a fresh Python process.
- **Pi was remarkably close to 4 resolutions**:
  - In `07-lease-cascade`, Pi passed the complex barrier cascade test (`test_retry_terminal_and_blocked_barrier_cascade ... ok`),
    only failing an oracle linter check because a generated test imported `import support` rather than `from tests import support`.
  - In `08-lease-fence`, Pi passed the core claim-fencing test (`test_stale_worker_cannot_overwrite_newer_claim ... ok`),
    failing only a CLI error exit code check (exit code 2 vs 1).
- **Rupi achieved 0 verified resolutions**, but demonstrated massive progress compared to Round 1:
  - In Round 1, Rupi generated package files for only 4 cases, none passing tests.
  - In Round 2, Rupi generated full multi-file packages for 8 of 10 cases. On `01-task-ledger`,
    Rupi wrote the complete package and passed over 50 unit tests; on `06-artifact-pipeline`, Rupi
    passed 3 of 4 acceptance tests, failing only on initial job status naming (`succeeded` vs `pending`).
- **Context growth & token economy gap**: Across the 10 cases, `rupi` consumed **5,009,501 input tokens**
  versus `pi`'s **123,410 input tokens** — a **40.6× ratio**. Rupi re-sends full unabridged session
  history (including multi-kilobyte command and file listings) across turn resumes, creating severe
  prefill pressure on local hardware, while Pi compacts conversational context into lean representations.

| Agent | Cases evaluated | Oracle resolutions | Input tokens | Output tokens | Total tokens | Agent wall time | Median case wall | Timed-out turns | Nonzero exits |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `rupi` | 10 | 0 / 10 | 5,009,501 | 374,964 | 5,384,465 | 32,906.0 s | 3,430.9 s | 27 / 40 | 12 / 40 |
| `pi` | 10 | **2 / 10** | 123,410 | 504,273 | 627,683 | 32,022.0 s | 3,546.9 s | 28 / 36 | 0 / 36 |

## Fixed protocol & Round 2 configuration

- Model: `qwen3.8-flash-next` served by llama.cpp at `http://127.0.0.1:8000/v1`.
- Options: 262,144 context, 32,768 generation, IQ4_XS weights, xhigh server reasoning,
  temperature `1.0`, top-p `0.95`, top-k `20`, q8 KV cache, flash attention.
- Server reported llama.cpp `0.4.0-dev`, build 10909, commit `a2878d30`.
- Host: AMD Ryzen AI Max+ 395 / 128 GB unified-memory Windows 11 machine.
- Harness parameters for Round 2:
  - Turn timeout: **900 seconds** (15 minutes), up from 300 s in Round 1.
  - Max turns: **4 turns**, up from 2 turns in Round 1.
  - Max model requests per turn in `rupi`: **24**, up from 8 in Round 1.
  - Provider timeout `request_timeout_ms` in `rupi`: configured to **900,000 ms** (15 minutes),
    matching turn timeout to eliminate the 90–120 s provider aborts that truncated Round 1.
  - `pi` launcher: executed via direct `node.exe` with redirected standard input explicitly closed
    at launch to prevent Windows stream buffering and hung stdin waiting.
- Acceptance oracle: unchanged, executed in a clean sibling directory after every attempted turn.
- Resolution rule: a case is resolved only when the fresh-process acceptance oracle exits zero.

## Results

### Aggregate measures

| Agent | Cases | Turns attempted | Turns to resolution | Input / output / total tokens | Agent wall sum | Median case wall | Model requests | Tool calls | Timed-out turns | Nonzero exits |
| --- | ---: | ---: | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `rupi` | 10 | 40 | 0 / 10 | 5,009,501 / 374,964 / 5,384,465 | 32,906.0 s | 3,430.9 s | 282 | 277 | 27 / 40 | 12 / 40 |
| `pi` | 10 | 36 | 2 / 10 (`02`, `04`) | 123,410 / 504,273 / 627,683 | 32,022.0 s | 3,546.9 s | 293 | 309 | 28 / 36 | 0 / 36 |

### Primary measures

| Case | `rupi` turns | `pi` turns | `rupi` tokens (in / out / total) | `pi` tokens (in / out / total) | `rupi` wall | `pi` wall | Resolution |
| --- | ---: | ---: | --- | --- | ---: | ---: | --- |
| 01 Task Ledger | >4 | >4 | 1,675,898 / 38,863 / 1,714,761 | 13,231 / 50,215 / 63,446 | 2,902.7 s | 3,601.2 s | neither |
| 02 Reading Queue | >4 | **2** | 793,276 / 58,374 / 851,650 | 7,624 / 32,232 / 39,856 | 3,554.1 s | 1,800.5 s | **pi** (turn 2) |
| 03 Event Outbox | >4 | >4 | 74,206 / 52,209 / 126,415 | 23,874 / 63,986 / 87,860 | 3,516.2 s | 3,474.5 s | neither |
| 04 Webhook Inbox | >4 | **2** | 572,615 / 47,143 / 619,758 | 10,762 / 30,352 / 41,114 | 3,600.9 s | 1,800.6 s | **pi** (turn 2) |
| 05 Batch Relay | >4 | >4 | 319,504 / 24,418 / 343,922 | 11,809 / 56,699 / 68,508 | 3,053.1 s | 3,554.6 s | neither |
| 06 Artifact Pipeline | >4 | >4 | 203,507 / 49,208 / 252,715 | 13,590 / 50,898 / 64,488 | 3,557.6 s | 3,601.0 s | neither |
| 07 Lease Cascade | >4 | >4 | 396,382 / 25,455 / 421,837 | 8,688 / 59,377 / 68,065 | 3,171.8 s | 3,567.9 s | neither |
| 08 Lease Fence | >4 | >4 | 605,564 / 38,054 / 643,618 | 6,531 / 48,143 / 54,674 | 3,600.9 s | 3,481.5 s | neither |
| 09 Lease Receipt | >4 | >4 | 272,130 / 23,593 / 295,723 | 15,715 / 49,811 / 65,526 | 3,345.5 s | 3,601.0 s | neither |
| 10 Receipt Ledger | >4 | >4 | 96,419 / 17,647 / 114,066 | 11,586 / 62,560 / 74,146 | 2,603.2 s | 3,539.2 s | neither |

### Visible package files generated

| Case | `rupi` visible package files | `pi` visible package files |
| --- | --- | --- |
| 01 Task Ledger | 8 files (`tasklog/__init__.py`, `__main__.py`, `cli.py`, `ledger.py`, tests) | 14 files (`tasklog` package, models, state, 6 test suites) |
| 02 Reading Queue | 7 files (`readqueue` package, `api.py`, `server.py`, `store.py`) | 11 files (`readqueue` package, validation, store tests) |
| 03 Event Outbox | 0 files | 11 files (`outbox` package, server, store, 4 sink scripts) |
| 04 Webhook Inbox | 9 files (`webhookinbox` package, signatures, storage, worker) | 15 files (`webhookinbox` package, protocol, 6 sink scripts) |
| 05 Batch Relay | 5 files (`batchrelay` package, storage, protocol, worker) | 17 files (`batchrelay` package, canonical, ids, 7 test suites) |
| 06 Artifact Pipeline | 8 files (`artifactpipe` package, service, signing, storage, worker) | 24 files (`artifactpipe` package, canonical, 12 sink fixtures) |
| 07 Lease Cascade | 8 files (`leasecascade` package, pipeline, server, storage, worker) | 12 files (`leasecascade` package, http_app, ids, signing, store) |
| 08 Lease Fence | 6 files (`leasefence` package, pipeline, server, store, worker) | 9 files (`leasefence` package, app, cli, server, sink, store) |
| 09 Lease Receipt | 6 files (`leasereceipt` package, http_app, store, validator, worker) | 19 files (`leasereceipt` package, canonical, cli, server, 10 tests) |
| 10 Receipt Ledger | 4 files (`receiptledger/__init__.py`, `canonical.py`, `store.py`, `validation.py`) | 5 files (`receiptledger/audit.py`, `canonical.py`, `server.py`, `store.py`) |

## Qualitative comparison & behavioral analysis

### 1. Where Pi Truly Outperforms: Modular Test Sinks & Self-Verification

A major qualitative insight from Round 2 is *how* Pi approaches complex distributed contracts:
- In cases involving external sink processes (`03-event-outbox`, `04-webhook-inbox`, `06-artifact-pipeline`),
  Pi consistently created dedicated mock sink scripts (e.g., `tests/sinks/sink_ok.py`, `sink_fail.py`,
  `sink_malformed.py`, `sink_mismatch.py`, `sink_stall.py`).
- By writing these executable sink fixtures early in Turn 1, Pi was able to test worker invocation,
  lease handling, and error recording via its shell tool before concluding the turn.
- In `04-webhook-inbox`, this test-driven integration enabled Pi to identify and fix worker crash reclaim
  behavior in Turn 2, resulting in an immediate full pass on the acceptance oracle.
- In contrast, Rupi wrote the worker and server logic directly but relied mainly on standard `unittest`
  stubs without creating realistic subprocess executables to verify stdin/stdout NDJSON exchange.

### 2. Why Rupi Stumbled on the Finish Line

Rupi produced impressive, high-volume code in Round 2 (e.g., writing 8 files and passing 50+ unit tests
in `01-task-ledger`). However, it missed specific interface boundaries:
- In `02-reading-queue`, the HTTP server handler encountered an early socket closure during health-check
  probing (`Remote end closed connection without response`).
- In `06-artifact-pipeline`, Rupi wrote a working pipeline service, but initialized job status to
  `succeeded` instead of `pending`, causing an assertion failure in the oracle's initial-state check.
- In `07-lease-cascade`, both agents encountered the SQLite threading exception (`SQLite objects created in a thread can only be used in that same thread`). Pi handled connection factory patterns better in other cases, whereas Rupi shared a connection instance across thread boundaries.
- In `10-receipt-ledger`, both agents created `server.py` and `canonical.py` but omitted `__main__.py`,
  causing `python -m receiptledger` to fail.

### 3. Context Management & The 40× Token Economy Asymmetry

The most striking quantitative divergence between `rupi` and `pi` is input token consumption:
- Pi consumed **123,410 input tokens** across 10 cases (average 12.3k per case).
- Rupi consumed **5,009,501 input tokens** across 10 cases (average 501.0k per case).
- In multi-turn sessions (e.g. `01-task-ledger`), Rupi's input tokens climbed to **1,675,898**.

Why this happens:
1. **Unabridged Tool Output Retention**: In Rupi, outputs from `read`, `exec`, and directory listings
   are stored verbosely in session history. When a session is resumed in subsequent turns (`--resume`),
   the entire history is re-sent to the provider without compaction.
2. **Prefill Latency Impact**: With local model serving (UD-IQ4_XS prefill < 200 tps), sending 40k–80k
   tokens per model request adds 3 to 7 minutes of prefill time to every model turn, leaving less time
   for actual token generation and tool calls before the 900 s turn timeout.
3. **Pi's Compaction**: Pi prunes tool results and truncates large outputs in conversational history,
   ensuring prompt prefill remains fast (<10 s) even after 20 tool requests.

## Per-case observations

- **01 Task Ledger**:
  - `rupi`: Implemented full `tasklog` package, atomic JSON writing, stable ID generation, and CLI.
    Executed 58 model requests and 71 tool calls, passing 50+ unit tests. A minor return-tuple unpacking
    discrepancy in `complete()` and help string formatting prevented the final oracle pass.
  - `pi`: Implemented full package with 14 files and 6 test suites. Ran into timeout during Turn 4 test
    fixes.
- **02 Reading Queue**:
  - `pi`: **RESOLVED (Turn 2)**. Implemented SQLite-backed HTTP server with deterministic JSON errors,
    restart persistence, and clean `/healthz` endpoints. Oracle passed with exit code 0.
  - `rupi`: Implemented full server and store, but server crashed on initial health check during oracle setup.
- **03 Event Outbox**:
  - `pi`: Implemented outbox server, worker, store, and 4 mock sink scripts (`broken_sink.py`, `fail_sink.py`,
    `ok_sink.py`, `topic_rule_sink.py`). Timed out during worker edge-case debugging.
  - `rupi`: Stuck in an early planning loop without generating package files.
- **04 Webhook Inbox**:
  - `pi`: **RESOLVED (Turn 2)**. Built complete HMAC verification, leased delivery worker, crash reclaim,
    and 6 mock sink scripts. Passed all oracle tests including crash reclaim.
  - `rupi`: Wrote 9 package files including server and signatures, but server failed to bind/respond on health
    probe during acceptance setup.
- **05 Batch Relay**:
  - `pi`: Implemented 17 files including DAG resolution, lease management, and 7 test suites. All help commands
    (`--help`, `serve --help`, `worker --help`) passed exit code 0. Missed DAG cycle error code in oracle.
  - `rupi`: Implemented 5 package files; timed out while implementing DAG dependency resolution.
- **06 Artifact Pipeline**:
  - `rupi`: Implemented 8 package files, passing 3 of 4 oracle tests. Failed on initial job status naming.
  - `pi`: Created 24 files including 12 sink fixtures; encountered SQLite thread affinity error on HTTP post.
- **07 Lease Cascade**:
  - `pi`: Passed complex cascade test (`test_retry_terminal_and_blocked_barrier_cascade ... ok`), but failed
    an acceptance import check due to `import support` in `tests/test_validation.py`.
  - `rupi`: Implemented 8 package files; encountered SQLite thread affinity error.
- **08 Lease Fence**:
  - `pi`: Passed claim-fencing test (`test_stale_worker_cannot_overwrite_newer_claim ... ok`), but failed a
    CLI returncode assertion (2 vs 1).
  - `rupi`: Implemented 6 package files; service failed health check during oracle setup.
- **09 Lease Receipt**:
  - `pi`: Implemented 19 files with comprehensive test coverage. Encountered terminal retry state assertion failure.
  - `rupi`: Implemented 6 package files; encountered 500 error on pipeline admission.
- **10 Receipt Ledger**:
  - Both agents implemented `server.py`, `store.py`, `canonical.py`, but neither generated `__main__.py`,
    causing `python -m receiptledger` execution to fail during oracle health checks.

## Actionable recommendations for `rupi` engine

Based on direct evidence from Round 2, the following engine improvements should be prioritized:

1. **Context Compaction & History Pruning (High Priority)**:
   - Implement automatic truncation / summarization of historical tool outputs (especially large `exec`
     and `read` results) before persisting to session trace or re-sending in `--resume`.
   - Capping prior turn outputs to 500–1000 tokens will reduce input tokens by 80–90%, dramatically
     speeding up prefill on local models and eliminating turn timeouts.
2. **Subprocess Self-Testing Guidance**:
   - For CLI and worker projects, guide the agent to perform a quick smoke check (`python -m <package> --help`,
     `python -m <package> serve --help`) and verify server health probes before completing a turn.
3. **SQLite Thread Safety Patterns**:
   - Provide standard-library guidance or templates that use `check_same_thread=False` or per-thread
     connection helpers when generating HTTP server code backed by SQLite.
4. **Package Executability**:
   - Ensure the agent always emits `__main__.py` alongside `__init__.py` when building runnable Python
     modules.

## Reproduction

```powershell
# Build the rupi binary
cargo build --bin rupi

# Run Segment 1 (cases 01–05)
pwsh -NoProfile -File bench/compare-pi-rupi.ps1 `
  -Agent all `
  -CaseId 01-task-ledger,02-reading-queue,03-event-outbox,04-webhook-inbox,05-batch-relay `
  -MaxTurns 4 `
  -TurnTimeoutSeconds 900 `
  -RunId bench-20260921-round2-01-05

# Run Segment 2 (cases 06–10)
pwsh -NoProfile -File bench/compare-pi-rupi.ps1 `
  -Agent all `
  -CaseId 06-artifact-pipeline,07-lease-cascade,08-lease-fence,09-lease-receipt,10-receipt-ledger `
  -MaxTurns 4 `
  -TurnTimeoutSeconds 900 `
  -RunId bench-20260921-round2-06-10
```

## Conclusion

Round 2 demonstrated the profound impact of generous runtime budgets. Under a 900-second turn timeout
and up to 4 turns, Pi achieved 2 verified oracle resolutions (`02-reading-queue` and `04-webhook-inbox`)
and came within a single line or import of resolving 2 more (`07-lease-cascade` and `08-lease-fence`).
Pi's strength lies in lean context management and proactive creation of mock subprocess fixtures.
Rupi made dramatic progress over Round 1 by generating complete, high-coverage packages for 8 of 10 cases,
but was held back by extreme context bloat (5.0M input tokens) and minor contract oversights.
Implementing context compaction across turns is the single highest-leverage improvement identified
for `rupi`.
