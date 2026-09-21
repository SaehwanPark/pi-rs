# Lease Fence fresh-memory observations

Status: complete as a tester case; model-authoring stopgate remains open
Date: 2026-09-20
Branch: `tester/2026-09-20-loop-7-lease-fence`
Base: `main` at `d98cf8d`
Draft PR: https://github.com/SaehwanPark/rupi/pull/115
Model: local `qwen3.8-flash-next` at `http://127.0.0.1:8000/v1`

This is the chronological evidence ledger. Model-written artifacts, tester
repairs, independent acceptance, trace/replay projections, and repository
checks are kept distinct.

## Case contract

- T: Lease Fence; see [`CASE_PLAN.md`](CASE_PLAN.md) and
  [`project/SPEC.md`](project/SPEC.md).
- Independent oracle:
  [`acceptance/test_lease_fence.py`](acceptance/test_lease_fence.py).
- Oracle boundary: fresh `python -m leasefence` service, worker, and sink
  processes; the oracle imports no project implementation module.
- Required gates: project unittest suite and independent fresh-process oracle,
  run separately.
- New difficulty: private claim-token fencing of stale worker completion after
  lease reclaim; the oracle runs two workers across the expiry race.
- Read-only requirement: `rupi trace` and `rupi replay` were used only on
  recorded sessions and did not contact the provider or execute historical
  tools.

## Exact artifacts and hashes

The prompts and configs used by the model are committed files. SHA-256 hashes
were collected with `Get-FileHash -Algorithm SHA256` from `project/`:

| Artifact | SHA-256 |
| --- | --- |
| `prompts/initial.txt` | `18F4341EA9123DEB1378C28B7377B11931C6FD54247F8FC97B5AEFB968389E98` |
| `prompts/recovery.txt` | `0F54C1079E9802E8950040DF8A439C4FE28D0772F3E6C1357C69B548A443C238` |
| `prompts/verify.txt` | `733329692C227A3887B4D2D0DDD8BBDC38A4DF5DE2644F72B62BAC985A2CB3CF` (not invoked) |
| `project/rupi.config.json` | `3CB1086E9B49A09CC8CF399382CB834B146375ED10134AC5CF8589C03BD0335B` |
| `project/rupi.recovery.config.json` | `60F918726D2B11FE522189890AAC0E56581715C75DA452E160E05936994DB079` |
| `project/rupi.verify.config.json` | `97E9B025523F5343511A22C13D24ED7942CB392EB2351AC0225E41B1FDF24CAB` |

The initial config bounded authoring at 6 model requests and 90,000 ms per
provider request. Recovery was bounded at 4 requests with the same request
deadline. Both used `thinking: low`, the local endpoint, and an opt-in
one-request progress boundary exposing `write`, `edit`, and `append`.

## Initial checkout and early PR

The repository was clean on `main`, with `HEAD` and `origin/main` both at
`d98cf8d` (`test: live case 6 Lease Cascade`). GitHub authentication was
available. The case branch was created with:

```text
git switch -c tester/2026-09-20-loop-7-lease-fence
```

The design/oracle baseline was committed and pushed before implementation:

```text
d0c497c test: define Loop 7 Lease Fence case
a7678bd test: close Loop 7 oracle service pipes
```

The first commit contained `CASE_PLAN.md`, `SPEC.md`, the independent oracle
and sink, `.gitignore`, configs, and exact prompts. The second commit closed
service stdout/stderr handles when the expected missing module caused an early
oracle process exit. That was an oracle-only warning cleanup, not a project or
rupi repair. Draft PR creation succeeded:

```text
gh pr create --repo SaehwanPark/rupi --base main \
  --head tester/2026-09-20-loop-7-lease-fence --draft \
  --title "test: live case 7 Lease Fence"
```

PR: https://github.com/SaehwanPark/rupi/pull/115

## Provider and binary preflight

The actual binary was present at:

```text
C:\Users\saehwan\repos\pi-rs\target\debug\rupi.exe
```

The direct provider check was:

```powershell
$sw = [Diagnostics.Stopwatch]::StartNew()
try {
  $response = Invoke-RestMethod -Uri 'http://127.0.0.1:8000/v1/models' -Method Get -TimeoutSec 10
  $sw.Stop()
  'provider_http=200'
  'elapsed_ms=' + $sw.ElapsedMilliseconds
  $response | ConvertTo-Json -Depth 5
} catch {
  $sw.Stop()
  'provider_error=' + $_.Exception.Message
  'elapsed_ms=' + $sw.ElapsedMilliseconds
  exit 1
}
```

Result: HTTP 200 in 78 ms. The response listed the exact model alias
`qwen3.8-flash-next`, `owned_by: llamacpp`, and a 262,144-token context
metadata value. The provider was healthy; no stopgate was raised.

## Pre-implementation oracle

The baseline command, run from the case root before any implementation, was:

```powershell
$sw = [Diagnostics.Stopwatch]::StartNew()
python -W error::ResourceWarning -m unittest discover -s acceptance -p 'test_*.py' -v
$code = $LASTEXITCODE
$sw.Stop()
"oracle_exit=$code elapsed_ms=$($sw.ElapsedMilliseconds)"
exit $code
```

The first run (before the oracle pipe cleanup) exited 1 after 6,482 ms. All
four tests failed during service startup with the expected missing-module
diagnostic, and Python also reported unclosed service pipes while unwinding.
The tester corrected only that oracle cleanup and pushed `a7678bd`.

The corrected baseline rerun exited 1 after 6,445 ms. All four tests failed
because the expected implementation was absent:

```text
No module named leasefence.__main__; 'leasefence' is a package and cannot be directly executed
```

This is the expected pre-implementation baseline, not an acceptance result.

## Bounded model authoring

### I-01 initial authoring

Exact command from `project/`:

```powershell
$prompt = Get-Content -LiteralPath '..\prompts\initial.txt' -Raw
$sw = [Diagnostics.Stopwatch]::StartNew()
$result = & 'C:\Users\saehwan\repos\pi-rs\target\debug\rupi.exe' run --config rupi.config.json --cwd . --prompt $prompt --no-color --no-reasoning --verbose 2>&1
$code = $LASTEXITCODE
$sw.Stop()
'exit_code=' + $code
'elapsed_ms=' + $sw.ElapsedMilliseconds
'--- combined stdout/stderr ---'
$result
exit $code
```

Session: `01a0c11c-6f16-7954-9d47-e8ba499bafaa`
Result: exit 1; 204,948 ms; 6 model-request starts and 6 completions;
`budget_exhausted`.

The model read `SPEC.md`, used a Windows `dir /b` probe, then wrote only
`project/leasefence/__init__.py`. It attempted to read
`..\acceptance\test_lease_fence.py`, which the runtime correctly rejected as
outside the configured workspace root. No functional package files, README, or
tests were created. The model's final response honestly marked the task
incomplete and did not claim verification.

The partial project command was:

```powershell
$sw = [Diagnostics.Stopwatch]::StartNew()
python -W error::ResourceWarning -m unittest discover -s tests -p 'test_*.py' -v
$code = $LASTEXITCODE
$sw.Stop()
"project_partial_exit=$code elapsed_ms=$($sw.ElapsedMilliseconds)"
exit $code
```

It exited 5 in 98 ms with `Ran 0 tests ... NO TESTS RAN`.

### R-01 recovery authoring

Exact command from the same partial `project/` workspace:

```powershell
$prompt = Get-Content -LiteralPath '..\prompts\recovery.txt' -Raw
$sw = [Diagnostics.Stopwatch]::StartNew()
$result = & 'C:\Users\saehwan\repos\pi-rs\target\debug\rupi.exe' run --config rupi.recovery.config.json --cwd . --prompt $prompt --no-color --no-reasoning --verbose 2>&1
$code = $LASTEXITCODE
$sw.Stop()
'exit_code=' + $code
'elapsed_ms=' + $sw.ElapsedMilliseconds
'--- combined stdout/stderr ---'
$result
exit $code
```

Session: `01a0c11f-be3d-7baf-9a44-bb8d5a640697`
Result: exit 1; 169,788 ms; 3 model-request starts and 3 completions; the
third request ended with the configured 90,000 ms provider timeout.

The model reread the specification, repeated a `dir /s /b` probe, attempted an
edit of a nonexistent `README.md` whose find/replace was identical, and then
timed out before writing a functional file. No additional project artifact was
created. This was a provider/runtime completion stopgate, not an unavailable
endpoint.

## Read-only trace and replay

The actual binary was used for every projection. Each trace and replay command
exited 0; neither command contacted Qwen or executed a recorded tool.

```text
rupi.exe trace 01a0c11c-6f16-7954-9d47-e8ba499bafaa --config rupi.config.json --tools --sequence --no-color --quiet
rupi.exe replay .rupi-state\sessions\01a0c11c-6f16-7954-9d47-e8ba499bafaa.trace.jsonl --tools --sequence
rupi.exe trace 01a0c11f-be3d-7baf-9a44-bb8d5a640697 --config rupi.recovery.config.json --tools --sequence --no-color --quiet
rupi.exe replay .rupi-state-recovery\sessions\01a0c11f-be3d-7baf-9a44-bb8d5a640697.trace.jsonl --tools --sequence
```

Observed projections:

| Session | Trace | Replay | Recorded tool outcome shown |
| --- | --- | --- | --- |
| I-01 | 1,448 lines read; 1 selected; exit 0 | exit 0 | reads, `dir /b`, one successful write, and the refused outside-root read |
| R-01 | 2,263 lines read; 1 selected; exit 0 | exit 0 | reads, `dir /s /b`, and the failed identical README edit |

The trace JSONL counts were independently checked for model events:

| Session | Models | Reasoning provenance | Failover events | Unknown tool events | Requests |
| --- | --- | --- | --- | --- | --- |
| I-01 | one: `local/qwen3.8-flash-next` | `native` | 0 | 0 | 6/6 |
| R-01 | one: `local/qwen3.8-flash-next` | `native` | 0 | 0 | 3/3 |

No backup model, model voting, or orchestration appeared in either trace.

## Tester repair boundary

After the two bounded model attempts, the tester repaired only this case:

- retained model-written `project/leasefence/__init__.py` as the only model
  artifact;
- added `__main__.py`, `cli.py`, `ids.py`, `storage.py`, `service.py`, and
  `worker.py` implementing the spec and token-conditional finalization;
- added `project/README.md`, `project/tests/__init__.py`, and seven focused
  project tests;
- strengthened the oracle with a no-private-token assertion in sink requests;
- kept the independent sink and all parent/runtime files unchanged.

The implementation's critical fencing path stores a fresh private token on
claim, clears it during reclaim, and requires both `status == 'leased'` and
the exact token in every finalization update. A stale update returns false to
the worker, produces `stale lease ignored`, exits non-zero, and does not alter
the current row.

No tester repair is model-completion evidence. The functional implementation,
README, and project tests are explicitly tester-authored.

## Independent gates after repair

Project gate, run from `project/`:

```text
python -W error::ResourceWarning -m unittest discover -s tests -p 'test_*.py' -v
```

Result: exit 0; 7 tests; `Ran 7 tests in 1.478s`; wrapper elapsed 1,596 ms.

Independent oracle, run separately from the case root:

```text
python -W error::ResourceWarning -m unittest discover -s acceptance -p 'test_*.py' -v
```

Result: exit 0; 4 tests; `Ran 4 tests in 5.268s`; wrapper elapsed 5,408 ms.

The stale-fencing test observed worker A's claim, waited for GET-triggered
expiry/reclaim, let fresh worker B succeed, released worker A, and verified:

- B's output label remained durable;
- attempts were exactly 2;
- A exited non-zero and reported stale lease;
- A's late output did not change status, output, attempts, lease, or error;
- a third worker made no additional sink call.

The normal oracle test also verified that the sink request log contained no
`lease_token`, barrier fan-in preserved declared order and selected fields,
invalid signatures/cycles were non-mutating, idempotent admission returned
200, conflicting content returned 409, and a restarted service returned the
same succeeded projection.

## Repository checks

These checks were run after the case-local implementation and all passed:

| Command | Result |
| --- | --- |
| `cargo fmt --all --check` | exit 0 |
| `cargo check -p rupi-core --all-features` | exit 0; 0.87 s |
| `cargo clippy --workspace --all-targets -- -D warnings` | exit 0; 2.19 s |
| `cargo test --workspace --quiet` | exit 0; all workspace test binaries passed |
| `cargo doc --workspace --no-deps` | exit 0; 3.46 s |
| `bash -lc './bench/startup.sh --json /tmp/rupi-loop7-startup.json'` | exit 0; cold 14.99 ms, warm mean 10.07 ms, median 10.24 ms, max 10.45 ms |
| `git diff --check` | exit 0 |

The case-only diff contains no rupi source, repository test, canonical design,
architecture, compatibility, roadmap, README, or manual changes. Generated
`.rupi-state*`, bytecode, temporary databases, and logs are ignored and were
not committed.

## Residual risks and stopgates

- The model-authoring stopgate remains open: neither bounded model turn wrote a
  complete project or passed either gate. Acceptance is tester-repaired only.
- The stale race uses a deliberately short one-second lease and fresh local
  processes. It proves the specified conditional-finalization race, not a
  general concurrent-worker scheduler or exactly-once external side effect.
- A stale sink may already have observed a request; fencing protects durable
  state from stale overwrite but cannot undo an external side effect.
- This fixture covers one local Qwen alias and one Windows host; it does not
  establish provider portability or performance portability.
- No schema migration, multi-user authorization, or multi-database behavior
  is within T.

No concrete environment stopgate was exhausted: provider, binary, Git push,
and draft PR were all available, and both independent acceptance gates pass.
