# Benchmark report: `rupi` vs `pi` (Round 2)

Status: In progress — Segment 1 (Cases 01–05) complete; Segment 2 (Cases 06–10) executing.

This report records Round 2 of the fresh-memory comparison of `rupi` and Pi 0.86.1
on the ten repository evaluation cases under [`cases/`](../../cases/). The fixed inference
server is llama.cpp serving Qwen 3.8 Flash Next (UD-IQ4_XS).

Execution window: 2026-09-21 through 2026-09-22 America/New_York.
- Segment 1 run ID: `bench-20260921-round2-01-05` (cases 01–05)
- Segment 2 run ID: `bench-20260921-round2-06-10` (cases 06–10, currently running)

Machine-readable results, traces, session logs, verification output, and scratch
workspaces are located under the ignored `.benchmark/runs/` directory.

## Executive summary (Segment 1 Progress)

In Round 1, neither agent resolved any case within the 2-turn / 300 s budget (0/10 for both).
In Round 2, with generous limits (900 s turn timeout, up to 4 turns, max 24 model requests/turn,
and unblocked stdin):

- **Pi achieved verified acceptance-oracle resolutions on 2 out of 5 cases in Segment 1**:
  - `02-reading-queue` resolved in **2 turns** (1,800.5 s, 39,856 total tokens).
  - `04-webhook-inbox` resolved in **2 turns** (1,800.6 s, 41,114 total tokens).
  Both cases passed their external acceptance oracles on fresh Python processes.
- **Rupi achieved 0 verified resolutions in Segment 1**, though it produced extensive,
  nearly complete implementations on `01-task-ledger`, `02-reading-queue`, and `04-webhook-inbox`
  (e.g., 50+ passing tests in `01-task-ledger` with minor remaining edge-case failures).
- **Token consumption asymmetry widened dramatically**: Across Segment 1, `rupi` consumed
  **3,435,503 input tokens** vs. `pi`'s **67,300 input tokens** (a **51.0× ratio**).
  On `04-webhook-inbox`, `rupi` used 572,615 input tokens vs. `pi`'s 10,762 (a **53.2× ratio**).

| Agent | Cases evaluated | Oracle resolutions | Input tokens | Output tokens | Total tokens | Agent wall time |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `rupi` | 5 / 10 | 0 / 5 | 3,435,503 | 221,007 | 3,656,510 | 16,627.0 s |
| `pi` | 5 / 10 | **2 / 5** | 67,300 | 233,484 | 300,784 | 14,231.4 s |

## Fixed protocol & Round 2 configuration

- **Model**: `qwen3.8-flash-next` via llama.cpp @ `http://127.0.0.1:8000/v1`
  - Quantization: `UD-IQ4_XS`
  - Context window: 262,144; max tokens: 16,384; reasoning: `xhigh` on server, `low` requested on client
- **Host**: AMD Ryzen AI Max+ 395 (128 GB unified memory), Windows 11
- **Limits**:
  - Turn timeout: **900 seconds** (15 minutes), vs 300 s in Round 1
  - Max turns: **4 turns**, vs 2 turns in Round 1
  - Max model requests per turn: **24**, vs 8 in Round 1
  - `request_timeout_ms` in `rupi`: configured to 900,000 ms to eliminate artificial 90–120 s provider aborts
  - `pi` launcher: direct invocation via `node.exe` with redirected standard input explicitly closed to prevent Windows stream-buffering hangs

## Segment 1 Results (Cases 01–05)

| Case | `rupi` turns | `pi` turns | `rupi` tokens (in/out/tot) | `pi` tokens (in/out/tot) | `rupi` wall (s) | `pi` wall (s) | Resolution |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| 01 Task Ledger | >4 | >4 | 1,675,898 / 38,863 / 1,714,761 | 13,231 / 50,215 / 63,446 | 2,902.7 | 3,601.2 | neither |
| 02 Reading Queue | >4 | **2** | 793,276 / 58,374 / 851,650 | 7,624 / 32,232 / 39,856 | 3,554.1 | 1,800.5 | **pi** (turn 2) |
| 03 Event Outbox | >4 | >4 | 74,206 / 52,209 / 126,415 | 23,874 / 63,986 / 87,860 | 3,516.2 | 3,474.5 | neither |
| 04 Webhook Inbox | >4 | **2** | 572,615 / 47,143 / 619,758 | 10,762 / 30,352 / 41,114 | 3,600.9 | 1,800.6 | **pi** (turn 2) |
| 05 Batch Relay | >4 | >4 | 319,504 / 24,418 / 343,922 | 11,809 / 56,699 / 68,508 | 3,053.1 | 3,554.6 | neither |

### Visible package files generated

| Case | `rupi` package files | `pi` package files |
| --- | --- | --- |
| 01 Task Ledger | 8 files (`tasklog/__init__.py`, `__main__.py`, `cli.py`, `ledger.py`, tests) | 14 files (`tasklog` package, models, state, 6 test suites) |
| 02 Reading Queue | 7 files (`readqueue` package, `api.py`, `server.py`, `store.py`) | 11 files (`readqueue` package, validation, store tests) |
| 03 Event Outbox | 0 files (remained in planning/reading loop) | 11 files (`outbox` package, server, store, 4 sink scripts) |
| 04 Webhook Inbox | 9 files (`webhookinbox` package, signatures, storage, worker) | 15 files (`webhookinbox` package, protocol, 6 sink scripts) |
| 05 Batch Relay | 5 files (`batchrelay` package, storage, protocol, worker) | 17 files (`batchrelay` package, canonical, ids, 7 test suites) |

## Key Findings & Qualitative Observations from Segment 1

1. **Resolution Gap Emerges under Generous Budgets**:
   - In Round 1, both agents were choked by the 300 s timeout before completing implementations.
   - Under 900 s per turn and 4 turns, Pi successfully passed all oracle tests on 2 of 5 cases (`02-reading-queue` and `04-webhook-inbox`). Both cases required setting up SQLite schemas, HTTP request routing, and subprocess crash/lease reclamation.
   - Rupi wrote substantial code for `01-task-ledger` (passing 50+ unit tests), `02-reading-queue`, `04-webhook-inbox`, and `05-batch-relay`, but missed critical runtime contracts (e.g. server health-endpoint readiness or return-value signatures) that prevented oracle passes.

2. **Context Growth & Token Consumption**:
   - Rupi exhibits massive context amplification across multi-turn sessions (averaging ~687k input tokens per case, reaching 1.67M on `01-task-ledger`). Rupi currently carries full unabridged tool call outputs (including verbose directory and file listings) across turn resumes.
   - Pi employs context management and compaction, maintaining a lean ~13k input token average per case while generating more output tokens (233k vs 221k).

3. **Execution Robustness on Windows**:
   - Both agents successfully avoided shell quoting pitfalls on Windows when using direct process execution.
   - Pi systematically created dedicated sink mock scripts (`tests/sinks/sink_*.py`) to test subprocess integration before completing turns, contributing to its success on `04-webhook-inbox`.

---
*Report will be updated upon completion of Segment 2.*
