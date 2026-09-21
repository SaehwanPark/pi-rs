# Loop 9 observations: Receipt Ledger

Status: complete. Both acceptance gates pass. Model authoring stopped at its
fixed budgets; the finished project is explicitly tester-repaired and is not
reported as model completion.

## Scope and target

- Base: `main` at `8d234cb` (`test: live case 8 Lease Receipt (#116)`)
- Branch: `tester/2026-09-20-loop-9-receipt-ledger`
- Case root: `docs/cases/2026-09-20-receipt-ledger/`
- Draft PR: https://github.com/SaehwanPark/rupi/pull/117
- Target: **Receipt Ledger**, a dependency-free Python 3 HTTP/SQLite pipeline
  worker that adds one dimension beyond Lease Receipt: an append-only,
  same-transaction, SHA-256 hash-chained operational audit ledger with a
  read-only verifier and tail projection.
- Scope check: `git diff --name-only main...HEAD` contains only paths below
  the case root. No rupi source, repository tests, canonical documents,
  `ROADMAP.md`, README, user manual, or parent integration document changed.

The audit ledger is evidence only. SQLite job state remains the side-effect
authority; private lease tokens remain separate from public delivery keys;
lost acknowledgements remain unknown; and the sink contract remains
at-least-once with an idempotent receipt protocol, not arbitrary exactly-once
delivery.

## Endpoint and binary verification

The local endpoint was verified before authoring and again before handoff with:

```powershell
Invoke-WebRequest -UseBasicParsing -Uri 'http://127.0.0.1:8000/v1/models'
```

The handoff check returned HTTP `200` in `40 ms`. The response contained model
`qwen3.8-flash-next`, `owned_by: llamacpp`, and `meta.n_ctx: 262144`.

The runtime binary used for both attempts was:

```text
C:\Users\saehwan\repos\pi-rs\target\debug\rupi.exe
```

It existed before the live runs. Both configurations selected only local
`qwen3.8-flash-next` at `http://127.0.0.1:8000/v1`; no backup model or remote
provider was configured.

## Fixed prompt and configuration provenance

SHA-256 values were computed over the exact UTF-8 bytes of the files:

| Artifact | SHA-256 |
| --- | --- |
| `prompts/initial.txt` | `E8296C59B2449A6DD141039476364BBF5B1A0A7FD2449C0CB79591CB29783644` |
| `prompts/recovery.txt` | `E68C5FC667C8ED56EA4524D3AFCAC45D258ED4D8798CC7462EE4AF556B0E6ED7` |
| `prompts/verify.txt` | `28789314BB6ECA4A9658ACEC5C3598080E24839A1103497887A8039404F1A344` |
| `project/rupi.config.json` | `3CB1086E9B49A09CC8CF399382CB834B146375ED10134AC5CF8589C03BD0335B` |
| `project/rupi.recovery.config.json` | `60F918726D2B11FE522189890AAC0E56581715C75DA452E160E05936994DB079` |
| `project/rupi.verify.config.json` | `97E9B025523F5343511A22C13D24ED7942CB392EB2351AC0225E41B1FDF24CAB` |

The initial budget was at most 6 model requests with a 90,000 ms provider
request timeout, `thinking: low`, and a 90,000 ms shell timeout. Recovery was
fixed at at most 4 requests with the same provider and shell timeouts. The
optional verify prompt/config was not used: after the two authoring stopgates,
the tester repair and independent gates supplied the required evidence, so no
third authoring phase was justified.

## Pre-implementation oracle

Before any project implementation, from the case root:

```powershell
python -W error::ResourceWarning -m unittest discover -s acceptance -p 'test_*.py' -v
```

Result: exit `1`, wrapper elapsed `6433 ms`, four tests failed at import with
`ModuleNotFoundError: No module named 'receiptledger'`. This is the expected
pre-implementation baseline.

## Initial model authoring

The exact runtime invocation was:

```powershell
$prompt = Get-Content -LiteralPath '..\prompts\initial.txt' -Raw
$sw = [Diagnostics.Stopwatch]::StartNew()
& 'C:\Users\saehwan\repos\pi-rs\target\debug\rupi.exe' run --config rupi.config.json --cwd . --prompt $prompt --no-color --no-reasoning --verbose 2>&1
$code = $LASTEXITCODE
$sw.Stop()
"initial_exit=$code elapsed_ms=$($sw.ElapsedMilliseconds)"
```

The short runtime process id was `34057`; the persistent rupi session id was
`01a0c165-0af3-77da-9d22-2202f2d6b75c`. It made 6 requests, exited `1`, and
the wrapper elapsed time was `206145 ms` (trace turn duration `206117 ms`).
Per-request trace timings and outcomes were:

| Request | Provider time | Outcome |
| ---: | ---: | --- |
| 1 | 13,284 ms | Read `SPEC.md`, succeeded |
| 2 | 42,987 ms | Wrote `receiptledger/__init__.py` and `receiptledger/__main__.py`, both succeeded |
| 3 | 29,307 ms | Read `SPEC.md` at offset 122, succeeded |
| 4 | 20,596 ms | Read `SPEC.md` at offset 226, succeeded |
| 5 | 15,353 ms | Executed `dir /b /s "%CD%" 2>nul | findstr /v "__pycache__"`, failed on the Windows tool boundary |
| 6 | 84,313 ms | Returned `stop` without completing implementation; request budget exhausted |

The only model-written files were the two entrypoint files above. No project
implementation, README, project tests, or oracle was model-completed. The
trace records native reasoning provenance and no backup-model activation.

## Recovery model authoring

The recovery invocation was identical except for the prompt and config:

```powershell
$prompt = Get-Content -LiteralPath '..\prompts\recovery.txt' -Raw
$sw = [Diagnostics.Stopwatch]::StartNew()
& 'C:\Users\saehwan\repos\pi-rs\target\debug\rupi.exe' run --config rupi.recovery.config.json --cwd . --prompt $prompt --no-color --no-reasoning --verbose 2>&1
$code = $LASTEXITCODE
$sw.Stop()
"recovery_exit=$code elapsed_ms=$($sw.ElapsedMilliseconds)"
```

The short runtime process id was `11902`; the persistent rupi session id was
`01a0c168-5615-763a-a5f3-32a0a4107f7d`. It made 2 requests, exited `1`, and
the wrapper elapsed time was `105559 ms` (trace turn duration `105525 ms`).

| Request | Provider time | Outcome |
| ---: | ---: | --- |
| 1 | 15,401 ms | Executed POSIX `ls -la; echo ... find ...`, which failed on Windows |
| 2 | 90,047 ms | Provider timeout/no finish reason; no tool result and no files written |

Recovery therefore stopped at the fixed provider stopgate. No verify-model turn
was run, and no extra request was made after the configured recovery budget.

## Tester repair boundary

After model stop, the tester repaired only files under this case directory.
The repair added the standard-library implementation and project checks:

- `project/receiptledger/audit.py`, `cli.py`, `ids.py`, `service.py`,
  `storage.py`, and `worker.py`;
- `project/README.md`, `project/tests/__init__.py`, and
  `project/tests/test_receiptledger.py`;
- `startup-loop9.json`.

Two small test-harness edits were made after real failures, both explicitly
tester repairs rather than model completion:

1. `project/tests/test_receiptledger.py` and
   `acceptance/test_receipt_ledger.py` now close direct SQLite connections with
   `contextlib.closing`; the first project gate exposed Windows file-handle
   cleanup errors under `-W error::ResourceWarning`.
2. `project/receiptledger/storage.py` changed the safe audit detail label from
   `claim_token_mismatch` to `stale_claim_rejected`. The first independent
   oracle correctly rejected the broader forbidden substring `claim_token` in
   audit output; the strict no-private-token assertion was retained, not
   weakened.

The model-created `__init__.py` and `__main__.py` were preserved, but the
remaining implementation, tests, README, and harness repair are tester-owned.

## Acceptance gates

Project gate, from `project/`:

```powershell
python -W error::ResourceWarning -m unittest discover -s tests -p 'test_*.py' -q
```

Result: `9 tests` passed, internal elapsed `1.642 s`, wrapper output
`project_final_gate_exit=0 elapsed_ms=1752`.

Independent fresh-process oracle, from the case root:

```powershell
python -W error::ResourceWarning -m unittest discover -s acceptance -p 'test_*.py' -q
```

Result: `4 tests` passed, internal elapsed `7.150 s`, wrapper output
`oracle_final_gate_exit=0 elapsed_ms=7279`.

The oracle starts service, worker, and sink as separate Python processes and
checks authenticated atomic admission, deterministic idempotency/conflict,
declared dependency/barrier data flow, retries, terminal failure and blocked
dependents, reclaim, stale fencing, lost-ack receipt replay, audit restart
verification, direct argv, help, standard-library-only imports, and independent
tamper detection. It recomputes the event sequence and SHA-256 chain directly
from SQLite; `audit --verify` is also run before and after restart and after a
tampered row is introduced.

## Trace and replay

Only read-only inspection commands were used against the authoring sessions:

```powershell
rupi.exe trace 01a0c165-0af3-77da-9d22-2202f2d6b75c --config rupi.config.json --tools --sequence --no-color --quiet --no-reasoning
rupi.exe replay .rupi-state/sessions/01a0c165-0af3-77da-9d22-2202f2d6b75c.trace.jsonl --tools --sequence
rupi.exe trace 01a0c168-5615-763a-a5f3-32a0a4107f7d --config rupi.recovery.config.json --tools --sequence --no-color --quiet --no-reasoning
rupi.exe replay .rupi-state-recovery/sessions/01a0c168-5615-763a-a5f3-32a0a4107f7d.trace.jsonl --tools --sequence
```

All four wrapper commands exited `0`. The initial trace read 1,036 entries and
showed the failed session projection; its replay exited `0` and showed only the
recorded read/write/read/read/failed-exec lifecycle. The recovery trace read
1,607 entries and showed its failed session projection; its replay exited `0`
and showed only the failed `ls` lifecycle. No historical tool was executed by
trace or replay. `.rupi-state*` remains ignored local runtime evidence, as
specified by the case plan; session ids, counts, outcomes, and paths are
preserved here.

## Repository verification and startup

All required proportionate checks passed from the repository root:

```text
cargo fmt --all --check                                  PASS
cargo check -p rupi-core --all-features                 PASS
cargo clippy --workspace --all-targets -- -D warnings   PASS
cargo test --workspace                                  PASS
cargo doc --workspace --no-deps                         PASS
git diff --check                                         PASS
```

Startup benchmark:

```powershell
bash -lc './bench/startup.sh --json docs/cases/2026-09-20-receipt-ledger/startup-loop9.json'
```

Passed with cold `9.736 ms`; warm minimum `8.190 ms`, mean `9.255 ms`, median
`9.309 ms`, maximum `10.089 ms` over 10 iterations. The case changed no
runtime startup path.

## Evidence commits

- `9e704fa` — `test: define Loop 9 Receipt Ledger case`
- `d5fa3c1` — `test: repair Loop 9 Receipt Ledger project`
- `73b546b` — `test: record Loop 9 Receipt Ledger evidence`
- `7d7b125` — `test: clean Loop 9 case documentation`
