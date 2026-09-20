# Final 120-second budget retry report: Webhook Inbox

Date: 2026-09-20
Case: **Webhook Inbox**
Branch: `tester/2026-09-20-loop-3-webhook-inbox`
Base commit: `5ab2dc9`

## Server, build, and isolation

The user-authorized local llama server remained in:

```text
--reasoning off
```

The healthy `/v1/models` response listed `qwen3.8-flash-next`, owned by
`llamacpp`, with context size 262,144. Rupi was rebuilt from `5ab2dc9`:

```text
cargo build --bin rupi
Finished `dev` profile [unoptimized + debuginfo]
```

The model cwd was a brand-new ignored workspace:

```text
_workspace/live-retries/2026-09-20-webhook-inbox-120s/project/
```

Before execution it contained only copied `SPEC.md`, the three configs, and
`.gitignore`. The unchanged `acceptance/blocking_sink.py`,
`acceptance/fake_sink.py`, and `acceptance/test_webhook_inbox.py` were copied
into the sibling `acceptance/` directory. No prior retry or implementation
files were copied, pre-created, or deleted. Initial and recovery state roots
were separate: `.rupi-state/` and `.rupi-state-recovery/`, both ignored.

The configs retained `thinking: low`, `request_timeout_ms: 120000`,
`max_model_requests_per_turn: 8` initially and `6` for recovery,
`max_model_requests_without_progress: 1`, and the progress allowlist
`write`, `edit`, `append`.

Each config was checked before execution with:

```text
rupi.exe trace --config <config> --session parse-probe --silent --no-color
```

All three exited `1` only because no `parse-probe` session existed; each
reported `0 sessions recorded`, with no invalid-config error.

The exact unchanged initial prompt and command from `OBSERVATIONS.md` ran
first. Because it remained incomplete, recovery was needed. The recovery
command used the committed recovery prompt block but accidentally appended this
sentence, which is not present in `OBSERVATIONS.md`:

```text
Treat the task as incomplete if any required file or verification is unfinished.
```

This is a prompt-fidelity deviation in R-14 and is retained explicitly rather
than presented as an exact-prompt result. Both typed deadlines and the recovery
request budget were allowed to finish naturally. No oracle, rupi source,
canonical doc, or project repair was used. No further recovery was run because
this was the final bounded retry.

## Retry results

| retry | session / config | wrapper elapsed (trace turn) | first model write | model requests started / completed / retries | tool requests / completions / failures | final status / exit |
| --- | --- | ---: | --- | ---: | ---: | --- |
| R-13 initial | `01a0c065-0a45-74db-89ad-efbfc70a510f` / `rupi.config.json` | 200,325 ms (200,252 ms) | 51,272 ms: `webhookinbox/__init__.py` | 5 / 5 / 1 | 5 / 5 / 0 | `turn_completed` `failed.kind=provider_unavailable`; exit `1` |
| R-14 recovery | `01a0c068-670a-795d-a220-ed5f8452f1d4` / `rupi.recovery.config.json` | 215,824 ms (215,790 ms) | 76,102 ms: `README.md` | 6 / 6 / 0 | 7 / 6 / 1 | `turn_completed` `budget_exhausted`; exit `1` |

The initial wrapper ran from `2026-09-20T19:57:22.3223118Z` to
`2026-09-20T20:00:42.6504680Z`. Recovery ran from
`2026-09-20T20:01:02.7077071Z` to `2026-09-20T20:04:38.5350658Z`.

Initial model writes were:

```text
seq 31  webhookinbox/__init__.py
seq 34  webhookinbox/signature.py
seq 64  webhookinbox/schema.py
```

Recovery model writes were:

```text
seq 38   README.md
seq 148  webhookinbox/__init__.py  (overwrote the initial file)
seq 151  webhookinbox/signing.py
```

## Boundary and tool evidence

R-13 exposed 7 schemas at request sequence `4`. After the initial `exec` and
`read(SPEC.md)`, the runtime appended the standard progress nudge at sequence
`27` and the info diagnostic at `28`. Request sequence `29` exposed only
`write`, `edit`, and `append`. After successful writes at sequences
`31`/`33` and `34`/`36`, request sequence `37` exposed all 7 schemas
again; later request sequences `67` and `71` also exposed 7. No second
boundary nudge or diagnostic appeared. The initial request then timed out at
120,000 ms (diagnostic sequence `69`), retried once, and ended in provider
quarantine (sequence `73`).

R-14 exposed 7 schemas at sequence `4`, appended the progress nudge at
`34` and info diagnostic at `35`, then exposed 3 schemas at `36`. After
the successful README write, sequence `41` exposed all 7 schemas again;
sequences `90` and `132` also exposed 7. Its finalization request at
`155` exposed 0 tools after the sixth request, followed by the budget
diagnostic at `1259`.

The recovery tool failure was an attempted read of
`C:\Users\saehwan\.agents\skills\preferred-workflow\SKILL.md`; rupi
rejected it because the path was outside the model workspace. The remaining
six recovery tool completions succeeded. This failure was not repaired or
hidden.

The boundary diagnostic text was:

```text
progress boundary active after 1 model request(s) without a configured progress tool; next request exposes write, edit, append
```

## Model-authored output and acceptance

The final workspace contained these model-authored implementation/documentation
files:

```text
README.md
webhookinbox/__init__.py
webhookinbox/schema.py
webhookinbox/signature.py
webhookinbox/signing.py
```

No `store.py`, `server.py`, `sink.py`, `cli.py`, `__main__.py`, or
`tests/` existed. The recovery model explicitly reported that the first
README was written from an incorrect assumption and contradicted SPEC.md, and
that implementation and verification were unfinished. No tester repair was
used.

Because model output existed, both unchanged acceptance checks were run:

```text
python.exe -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v
exit 1; ImportError: Start directory is not importable: 'tests'
```

```text
python.exe -W error::ResourceWarning -m unittest discover -s acceptance -p "test_*.py" -v
exit 1; Ran 2 tests in 16.642s; both failed during setup because /healthz
connection attempts were refused (WinError 10061)
```

## Trace and replay

All required read-only projections exited `0`:

```text
rupi.exe trace --config rupi.config.json --session 01a0c065-0a45-74db-89ad-efbfc70a510f --tools --sequence --no-color --no-reasoning
exit 0; 75 entries read; 15 tool lifecycle frames shown
rupi.exe replay .rupi-state/sessions/01a0c065-0a45-74db-89ad-efbfc70a510f.trace.jsonl --tools --sequence
exit 0; 15 tool lifecycle frames replayed

rupi.exe trace --config rupi.recovery.config.json --session 01a0c068-670a-795d-a220-ed5f8452f1d4 --tools --sequence --no-color --no-reasoning
exit 0; 1,261 entries read; 20 tool lifecycle frames shown
rupi.exe replay .rupi-state-recovery/sessions/01a0c068-670a-795d-a220-ed5f8452f1d4.trace.jsonl --tools --sequence
exit 0; 20 tool lifecycle frames replayed
```

## Finding

The high implementation stopgate **remains**. The longer 120-second request
deadline and six-request recovery budget allowed more model-authored files and
first writes, while the one-shot boundary continued to return normal schemas
after successful progress. However, the model still did not complete the
package, produced an incorrect README, created no tests, and failed both
acceptance checks. The remaining stopgate is therefore bounded model/provider
completion, not boundary reactivation. This was the final implementation retry;
no further Webhook Inbox retry should be performed.
