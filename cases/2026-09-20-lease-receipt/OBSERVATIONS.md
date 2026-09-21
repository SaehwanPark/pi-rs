# Lease Receipt fresh-memory observations

Status: complete as a tester case; model-authoring stopgate remains open
Date: 2026-09-20
Base: `main` at `1e4030d` (`test: live case 7 Lease Fence (#115)`)
Branch: `tester/2026-09-20-loop-8-lease-receipt`
Draft PR: https://github.com/SaehwanPark/rupi/pull/116
Model: local `qwen3.8-flash-next` at `http://127.0.0.1:8000/v1`

This is the chronological evidence ledger. Model-written artifacts, tester
repairs, independent acceptance, trace/replay projections, and repository
checks are kept distinct.

## Case contract

- T: Lease Receipt; see [`CASE_PLAN.md`](CASE_PLAN.md) and
  [`project/SPEC.md`](project/SPEC.md).
- Independent oracle:
  [`acceptance/test_lease_receipt.py`](acceptance/test_lease_receipt.py).
- Oracle boundary: fresh `python -m leasereceipt` service, worker, and sink
  processes; the oracle imports no project implementation module.
- Required gates: project unittest suite and independent fresh-process oracle,
  run separately.
- New difficulty: a stable public delivery key and durable sink receipt let a
  fresh worker recover a side effect whose first acknowledgement was lost,
  while private claim-token fencing still prevents stale finalization.
- Read-only requirement: `rupi trace` and `rupi replay` were used only on
  recorded model sessions and did not contact the provider or execute
  historical tools.

## Exact artifacts and hashes

SHA-256 hashes collected with `Get-FileHash -Algorithm SHA256`:

| Artifact | SHA-256 |
| --- | --- |
| `prompts/initial.txt` | `B1C7A51478C5B45077D3FA9350C1EF5BB0B05E956C1CA415C8076EB99064782E` |
| `prompts/recovery.txt` | `B70A272B51F51BB0DB3E9AC42631CA48C3AAC26D878223EDD43F05EC32DDEBAB` |
| `prompts/verify.txt` | `955DFFF0221213BB365DA9182A4FEEE93C6BC3209EFCAAF95EE83C78CBD550C9` |
| `project/rupi.config.json` | `ACF46B2BF63B2C4AF93DBBF91013AEA9F49F73757B6F19AA8B3964F6C307465D` |
| `project/rupi.recovery.config.json` | `70B00CA643175C90857452C754DCAB33A3A92322C52FBCCB048654ECA03523F1` |
| `project/rupi.verify.config.json` | `78A866A64CA974362FE6FFF5F86627425AE80584D7E1C74BF6B97CBBEA613B07` |

The initial config bounded authoring at six model requests and 90,000 ms per
provider request. Recovery was bounded at four requests with the same timeout.
Both used `thinking: low`, the local endpoint, and the opt-in one-request
progress boundary exposing `write`, `edit`, and `append`. The verify config was
not invoked because tester implementation completed the acceptance gates after
the justified recovery attempt.

## Initial checkout, branch, and early PR

The working tree was clean on `main`, with `HEAD` and `origin/main` both at
`1e4030d`. The branch was created immediately with:

```text
git switch -c tester/2026-09-20-loop-8-lease-receipt
git push --set-upstream origin tester/2026-09-20-loop-8-lease-receipt
```

An initial draft-PR attempt before the first commit was rejected by GitHub with
`No commits between main and tester/2026-09-20-loop-8-lease-receipt`. After the
case baseline commit was pushed, the draft PR was created early:

```text
gh pr create --draft --base main \
  --head tester/2026-09-20-loop-8-lease-receipt \
  --title "test: live case 8 Lease Receipt"
```

PR: https://github.com/SaehwanPark/rupi/pull/116 (open, draft, base `main`).

## Provider and binary preflight

The actual binary was present at:

```text
C:\Users\saehwan\repos\pi-rs\target\debug\rupi.exe
```

The direct provider check was `Invoke-RestMethod` against
`http://127.0.0.1:8000/v1/models` with a 10-second timeout. Result: HTTP 200
in 69 ms. The endpoint listed `qwen3.8-flash-next`, `owned_by: llamacpp`, and
`n_ctx: 262144`. The provider was healthy; no environment stopgate was raised.

## Pre-implementation oracle

Command from the case root:

```powershell
$sw=[Diagnostics.Stopwatch]::StartNew()
python -W error::ResourceWarning -m unittest discover -s acceptance -p 'test_*.py' -v
$code=$LASTEXITCODE
$sw.Stop()
"oracle_exit=$code elapsed_ms=$($sw.ElapsedMilliseconds)"
exit $code
```

Result before implementation: exit 1 after 8,522 ms; all five tests failed at
the expected boundary because `python -m leasereceipt` was unavailable:
`No module named leasereceipt`. This was a baseline, not an acceptance result.

## Bounded model authoring

### I-01 initial authoring

Exact command from `project/`:

```powershell
$prompt = Get-Content -LiteralPath '..\prompts\initial.txt' -Raw
$sw = [Diagnostics.Stopwatch]::StartNew()
$result = & 'C:\Users\saehwan\repos\pi-rs\target\debug\rupi.exe' run --config rupi.config.json --cwd . --prompt $prompt --no-color --no-reasoning --verbose 2>&1
$code = $LASTEXITCODE
$sw.Stop()
$result | Set-Content -LiteralPath '..\initial-model-output.log' -Encoding utf8
'exit_code=' + $code
'elapsed_ms=' + $sw.ElapsedMilliseconds
exit $code
```

Session: `01a0c13d-ca84-7001-a696-0a32253c1059`.

Result: exit 1; 101,485 ms; two model-request starts and two completions. The
first request completed in 11,321 ms after one read of `SPEC.md`; the second
ended at the 90,000 ms provider timeout. The model wrote no project file,
README, or test. The initial request budget was six; only two requests were
consumed before the provider failure.

### R-01 recovery authoring

Recovery was justified because I-01 left the project empty. Exact command from
the same `project/` workspace:

```powershell
$prompt = Get-Content -LiteralPath '..\prompts\recovery.txt' -Raw
$sw = [Diagnostics.Stopwatch]::StartNew()
$result = & 'C:\Users\saehwan\repos\pi-rs\target\debug\rupi.exe' run --config rupi.recovery.config.json --cwd . --prompt $prompt --no-color --no-reasoning --verbose 2>&1
$code = $LASTEXITCODE
$sw.Stop()
$result | Set-Content -LiteralPath '..\recovery-model-output.log' -Encoding utf8
'exit_code=' + $code
'elapsed_ms=' + $sw.ElapsedMilliseconds
exit $code
```

Session: `01a0c13f-9f41-74e4-b3f6-624171aae11d`.

Result: exit 1; 101,988 ms; two model-request starts and two completions. The
first completed in 11,760 ms after reading `SPEC.md` and running a recursive
directory probe; the second ended at the 90,000 ms provider timeout. The
recovery model also wrote no project file. The recovery budget was four
requests; only two were consumed before the provider failure.

Neither attempt used a backup model, voting, delegation, or orchestration. No
model-created implementation artifact exists, so model completion is not
claimed. No bounded verify-model turn was needed or run.

## Read-only trace and replay

The actual binary was used for every projection. Each command exited 0; neither
command contacted Qwen or re-executed a recorded tool.

```text
rupi.exe trace 01a0c13d-ca84-7001-a696-0a32253c1059 --config rupi.config.json --tools --sequence --no-color --quiet
rupi.exe replay .rupi-state\sessions\01a0c13d-ca84-7001-a696-0a32253c1059.trace.jsonl --tools --sequence
rupi.exe trace 01a0c13f-9f41-74e4-b3f6-624171aae11d --config rupi.recovery.config.json --tools --sequence --no-color --quiet
rupi.exe replay .rupi-state-recovery\sessions\01a0c13f-9f41-74e4-b3f6-624171aae11d.trace.jsonl --tools --sequence
```

| Session | Trace entries | Replay selected | Models | Provenance | Failover | Unknown tool events |
| --- | ---: | ---: | --- | --- | ---: | ---: |
| I-01 | 1,286 | 3 tool lines | one local Qwen | native | 0 | 0 |
| R-01 | 1,234 | 6 tool lines | one local Qwen | native | 0 | 0 |

Trace event counts were also checked directly from the JSONL: each session had
two `model_request_started`, two `model_request_completed`, one epoch, and no
failover or unknown event. I-01 had one read tool call; R-01 had one read and
one directory-probe exec. Replay output contained only those recorded actions.

## Tester repair boundary

The model produced no implementation. After both bounded model attempts, all
functional project artifacts were tester-authored inside this case:

- `project/leasereceipt/` — standard-library HTTP service, SQLite store,
  validation, direct-argv worker, private-token fencing, stable delivery keys,
  and durable sink receipts;
- `project/tests/` — eight focused project tests;
- `project/README.md` — commands, signing, state, fencing, and receipt
  recovery documentation.

The independent oracle started as a tester-authored fresh-process fixture. Two
oracle-only repairs were needed and are not model-completion evidence:

1. `2d040c3` added logged `fan_in`/`inputs`, included the delivery key in the
   fixture's rejection responses, and corrected an assertion to account for the
   same bounded worker continuing to the child after retry recovery.
2. `0dc40e1` added explicit helper-process cleanup after a repeated oracle run
   exposed orphaned blocking sink children. The cleanup uses direct `taskkill`
   argv on Windows (or `SIGTERM` elsewhere); the follow-up run left zero
   case-local sink processes.

No rupi source, repository test, canonical document, roadmap, README, user
manual, or parent integration document was changed.

## Independent gates after repair

Final project gate, from `project/`:

```text
python -W error::ResourceWarning -m unittest discover -s tests -p 'test_*.py' -q
```

Result: exit 0; 8 tests; 1,670 ms wrapper elapsed.

Final independent oracle, from the case root:

```text
python -W error::ResourceWarning -m unittest discover -s acceptance -p 'test_*.py' -q
```

Result: exit 0; 5 tests; 7,665 ms wrapper elapsed. A prior repeated run also
passed; after the cleanup repair, a process check reported
`case_sink_orphan_count=0`.

The receipt-recovery test observed a fresh worker after the first worker was
killed while the sink had already persisted its receipt. The second request
used the same `receipt:source` delivery key, replayed the stored receipt, and
left one `applied=true` log entry plus one `replayed=true` entry. Attempts were
exactly two, the durable output/receipt survived, and no private token appeared
in the sink logs or HTTP projection.

## Invariant review

[`INVARIANT_REVIEW.md`](INVARIANT_REVIEW.md) records three read-only review
passes with verdict `pass` and no actionable findings. The review confirmed:

- canonical SQLite state controls side effects; trace/replay are projections;
- stale finalization requires the current private token and cannot alter a
  newer output or receipt;
- stable delivery identity is separate from private claim identity;
- one local model and native reasoning provenance were preserved in both traces;
- no startup/runtime source boundary was changed.

The remaining risk is intentionally explicit: a sink that ignores the stable
delivery key may observe duplicate requests, so this is not a general
exactly-once guarantee.

## Repository verification

Commands run from the repository root:

| Command | Result |
| --- | --- |
| `cargo fmt --all --check` | exit 0 |
| `cargo check -p rupi-core --all-features` | exit 0; finished in 0.13 s |
| `cargo clippy --workspace --all-targets -- -D warnings` | exit 0; finished in 0.48 s |
| `cargo test --workspace` | exit 0; all workspace test binaries and doctests passed |
| `cargo doc --workspace --no-deps` | exit 0; finished in 2.45 s |
| `bash bench/startup.sh --json docs/cases/2026-09-20-lease-receipt/startup-loop8.json` | initial direct `bash` lacked `cargo` (exit 127); rerun via `bash -lc` exit 0 |
| startup result | cold 11.058 ms; warm mean 10.987 ms; median 10.949 ms; max 11.629 ms |
| `git diff --check` | exit 0 |

The startup JSON is committed under this case directory, not under repository
benchmark state.
