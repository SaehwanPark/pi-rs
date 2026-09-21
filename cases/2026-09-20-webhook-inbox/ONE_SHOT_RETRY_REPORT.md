# Final one-shot progress-boundary retry report: Webhook Inbox

Date: 2026-09-20
Case: **Webhook Inbox**
Branch: `tester/2026-09-20-loop-3-webhook-inbox`
Base commit: `8efea83`

## Server and build

The user-authorized local llama server was healthy in isolated:

```text
--reasoning off
```

`GET http://127.0.0.1:8000/v1/models` returned `qwen3.8-flash-next`, owned by
`llamacpp`, with context size 262,144. The model, port, context, sampling, and
tool setup were otherwise unchanged.

The tester verified `HEAD=8efea83` and ran:

```text
cargo build --bin rupi
Finished `dev` profile [unoptimized + debuginfo]
```

No rupi Rust source, prompt, oracle, or canonical document was changed.

## Isolation and config parse

The model cwd was a brand-new ignored workspace:

```text
_workspace/live-retries/2026-09-20-webhook-inbox-one-shot/project/
```

Before execution it contained only copied `SPEC.md`,
`rupi.config.json`, `rupi.recovery.config.json`, `rupi.verify.config.json`, and
`.gitignore`. The unchanged `acceptance/blocking_sink.py`,
`acceptance/fake_sink.py`, and `acceptance/test_webhook_inbox.py` were copied
into the sibling `acceptance/` directory. The prior tester implementation and
all prior retry workspaces were excluded. No implementation file was
pre-created or deleted. Initial and recovery state roots were separate and
ignored: `.rupi-state/` and `.rupi-state-recovery/`.

Each config was checked before the live run with:

```text
rupi.exe trace --config <config> --session parse-probe --silent --no-color
```

All three commands exited `1` only because no `parse-probe` session existed;
each reported `0 sessions recorded`, with no `invalid config` error. The live
configs retained `thinking: low`, `request_timeout_ms: 60000`, initial/recovery
request limits `8`/`3`, `max_model_requests_without_progress: 1`, and the
progress allowlist `write`, `edit`, `append`.

The exact initial prompt and command from `OBSERVATIONS.md` were run first.
Because that turn left a partial model result, the exact recovery prompt and
recovery command were then run verbatim. Both typed deadlines were allowed to
finish naturally; no Ctrl-C, tester repair, or implementation-file cleanup was
used.

## Retry results

| retry | session / config | wall time | first project write | model requests started / completed | tool requests / completions / failures | final status / exit |
| --- | --- | ---: | --- | ---: | ---: | --- |
| R-11 initial | `01a0c056-6029-729d-a93b-af8b2d7e09ac` / `rupi.config.json` | 107,129 ms (trace turn 107,064 ms) | 30,149 ms: `write(webhookinbox/__init__.py)` | 5 / 5; 1 retry | 5 / 5 / 0 | `turn_completed` `failed.kind=provider_unavailable`; exit `1` |
| R-12 recovery | `01a0c058-4a7e-7bc7-8605-0fb1195a0f74` / `rupi.recovery.config.json` | 68,570 ms (trace turn 68,498 ms) | 25,833 ms: `write(webhookinbox/__init__.py)` | 3 / 3; no model retry | 3 / 3 / 0 | `turn_completed` `budget_exhausted`; exit `1` |

The initial wrapper ran from `2026-09-20T19:41:21.2684732Z` to
`2026-09-20T19:43:08.4028268Z`. Recovery ran from
`2026-09-20T19:43:26.7906194Z` to `2026-09-20T19:44:35.3661875Z`.

## One-shot boundary evidence

The initial trace shows the intended one-shot behavior:

- request sequence `4` exposed 7 schemas;
- after `exec(dir /a)` and `read(SPEC.md)`, the runtime appended the exact
  progress nudge at sequence `26` and the info diagnostic at `27`;
- request sequence `28` exposed only 3 schemas: `write`, `edit`, `append`;
- the model successfully wrote `__init__.py` at sequence `30` (completion
  `32`) and `package_info.py` at sequence `33` (completion `35`);
- the next request, sequence `36`, exposed all 7 schemas again. The later
  `read(SPEC.md)` at sequence `38` did not produce a second nudge or boundary
  diagnostic, proving that the successful progress tool completed the
  one-shot boundary and that a successful write did not reactivate it;
- sequences `41` and `45` also exposed 7 schemas. Sequence `42` timed out at
  60,016 ms; sequence `43` recorded the typed timeout, sequence `44` recorded
  retry 2/2, and sequence `47` recorded provider quarantine.

The recovery trace shows the same initial transition:

- request sequence `4` exposed 7 schemas;
- the nudge was sequence `10`, with the info diagnostic at `11`;
- request sequence `12` exposed `write`, `edit`, `append` only;
- successful writes were `__init__.py` at sequence `14` (completion `16`) and
  `__main__.py` at sequence `17` (completion `19`);
- the configured third request was the typed finalization request at sequence
  `21`, exposing 0 tools. The budget diagnostic was sequence `540`, followed
  by `turn_completed` `budget_exhausted` at `541` and session end at `542`.

The nudge text was:

```text
Runtime progress boundary: this implementation turn has spent the configured inspection budget without calling a progress tool. In your next response, call one of write, edit, append to make the requested change. Do not spend another request reading, probing, or planning; the turn remains incomplete until the change is attempted.
```

The boundary info diagnostic was:

```text
progress boundary active after 1 model request(s) without a configured progress tool; next request exposes write, edit, append
```

## Model-authored output and acceptance

The model authored only these project files:

```text
webhookinbox/__init__.py       (initially written, then overwritten in recovery)
webhookinbox/package_info.py   (initial turn)
webhookinbox/__main__.py       (recovery turn)
```

`webhookinbox/` therefore exists, but the functional package is incomplete.
`README.md`, `tests/`, `cli.py`, `model.py`, `storage.py`, `server.py`, and
`worker.py` were absent. The model itself reported those files and verification
were unfinished. Python-created ignored cache files, if any, are not model
implementation output.

Because model output existed, the unchanged checks were run against this fresh
partial workspace without repair.

Project suite, from `project/`:

```text
python.exe -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v
exit 1; ImportError: Start directory is not importable: 'tests'
```

Independent oracle, from the retry root:

```text
python.exe -W error::ResourceWarning -m unittest discover -s acceptance -p "test_*.py" -v
exit 1; Ran 2 tests in 16.644s; both failed during setup because /healthz
connection attempts were refused (WinError 10061)
```

The oracle remained unchanged and was not run against the committed tester
repair.

## Trace and replay

All material retry projections were read-only and exited `0`:

```text
rupi.exe trace --config rupi.config.json --session 01a0c056-6029-729d-a93b-af8b2d7e09ac --tools --sequence --no-color --no-reasoning
exit 0; 49 entries read; 15 tool lifecycle frames shown
rupi.exe replay .rupi-state/sessions/01a0c056-6029-729d-a93b-af8b2d7e09ac.trace.jsonl --tools --sequence
exit 0; 15 tool lifecycle frames replayed

rupi.exe trace --config rupi.recovery.config.json --session 01a0c058-4a7e-7bc7-8605-0fb1195a0f74 --tools --sequence --no-color --no-reasoning
exit 0; 542 entries read; 9 tool lifecycle frames shown
rupi.exe replay .rupi-state-recovery/sessions/01a0c058-4a7e-7bc7-8605-0fb1195a0f74.trace.jsonl --tools --sequence
exit 0; 9 tool lifecycle frames replayed
```

## Finding

The high rupi implementation stopgate **remains**, but is materially reduced.
The one-shot progress fix is verified: after successful progress writes, the
normal tool set returned and the boundary did not reactivate. The model also
made first writes in both turns. However, it still failed to complete the
bounded package, README, and tests; the project suite and independent oracle
failed, and the initial turn ended in timeout/provider quarantine while
recovery ended at its request budget. This is not a successful model
implementation and no tester repair was used.
