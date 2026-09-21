# Final environment-only retry report: Webhook Inbox

Date: 2026-09-20
Case: **Webhook Inbox**
Branch: `tester/2026-09-20-loop-3-webhook-inbox`
Base commit: `20a311f`

## Server and build

This retry used the user-authorized isolated local llama server mode:

```text
--reasoning off
```

The endpoint was healthy before execution. `GET http://127.0.0.1:8000/v1/models`
returned model `qwen3.8-flash-next`, owned by `llamacpp`, with context size
262,144. The same model, port, context, sampling, and tools were otherwise
unchanged from the preceding retry.

Rupi was rebuilt from `20a311f` with `cargo build --bin rupi`; the build
succeeded.

## Workspace isolation and config parse

The exact initial and recovery prompt blocks and command shapes from
`OBSERVATIONS.md` were reused without modification. No prompt, oracle, rupi
Rust source, or canonical documentation was changed by the tester.

The model cwd was a brand-new ignored workspace:

```text
_workspace/live-retries/2026-09-20-webhook-inbox-reasoning-off/project/
```

It contained only copied `SPEC.md`, the three configs, and `.gitignore`. The
three unchanged oracle/sink files were copied into sibling `acceptance/`.
Prior implementation files and all prior retry workspaces were excluded. No
implementation file was pre-created or deleted. Initial and recovery state
roots were separate and ignored: `.rupi-state/` and `.rupi-state-recovery/`.

Before execution, each config was parsed through:

```text
rupi.exe trace --config <config> --session parse-probe --silent --no-color
```

All three exited `1` with `no session id starts with 'parse-probe'; 0 sessions
recorded, newest none`; none returned `invalid config`.

## Retry results

Both rupi processes were allowed to reach typed provider outcomes naturally; no
Ctrl-C was sent. The recovery prompt was run verbatim after the initial partial
model output; no tester deletion or repair occurred.

| retry | session / config | elapsed wall time | first project write | model requests started / completed | tool requests / completions / failures | schemas and boundary | exact exit/status |
| --- | --- | ---: | --- | ---: | ---: | --- | --- |
| R-09 initial | `01a0c04a-d453-733d-9581-280c626a7a47` / `rupi.config.json` | 117,161 ms (trace turn 117,072 ms) | `40,237 ms`; `write(webhookinbox/__init__.py)` | 5 / 5; 1 model retry | 4 / 4 / 0 | 7 -> 3 twice; two nudges/diagnostics | exit `1`; first request timeout, retry provider-quarantine; final `turn_completed` `failed.kind=provider_unavailable` |
| R-10 recovery | `01a0c04d-093f-71e2-a55c-60a6a76f699d` / `rupi.recovery.config.json` | 67,192 ms (trace turn 67,130 ms) | none; retained only the initial run's file | 3 / 3; 1 model retry | 2 / 2 / 0 | 7 -> 3 once; one nudge/diagnostic | exit `1`; request timeout, retry provider-quarantine; final `turn_completed` `failed.kind=provider_unavailable` |

The initial process ran from `2026-09-20T19:28:44.5574723Z` to
`2026-09-20T19:30:41.7235200Z`. Recovery ran from
`2026-09-20T19:31:09.1950175Z` to `2026-09-20T19:32:16.3921218Z`.

## Boundary evidence

R-09 exposed 7 tool schemas at request sequence `4`. Its first request called
`exec(dir)` and `read(SPEC.md)`. The runtime appended the exact progress nudge
at sequence `26` and emitted the info diagnostic at `27`; the next request at
sequence `28` exposed 3 schemas: `write`, `edit`, and `append`. The model then
requested `write` at sequence `30`, completing it at `32`; this was the first
project write, 40,237 ms after the session started.

After that progress, the model read more of `SPEC.md`. The boundary activated a
second time at nudge sequence `53` / diagnostic sequence `54`; request sequence
`55` again exposed 3 schemas. No further progress call occurred before the
60-second request timeout at diagnostic sequence `57`. The retry then produced
the typed provider-quarantine diagnostic at `61`.

R-10 exposed 7 schemas at request sequence `4`, called only `exec(dir)` and
`read(SPEC.md)`, appended the nudge at sequence `27`, emitted the diagnostic at
`28`, and exposed 3 schemas at request sequence `29`. It made no write before
the 60-second timeout and subsequent provider-quarantine retry.

The durable nudge text was:

```text
Runtime progress boundary: this implementation turn has spent the configured inspection budget without calling a progress tool. In your next response, call one of write, edit, append to make the requested change. Do not spend another request reading, probing, or planning; the turn remains incomplete until the change is attempted.
```

The durable info diagnostic was:

```text
progress boundary active after 1 model request(s) without a configured progress tool; next request exposes write, edit, append
```

## Model-authored result and acceptance checks

The model authored exactly one implementation file:

```text
webhookinbox/__init__.py
```

`README.md` and `tests/` were absent. Python-created ignored `__pycache__`
artifacts are not implementation output.

The exact project suite was run against this fresh workspace, without repair:

```text
python.exe -W error::ResourceWarning -m unittest discover -s tests -p test_*.py -v
exit 1; ImportError: Start directory is not importable: 'tests'
```

Because model output existed, the unchanged copied oracle was also run against
this workspace, not the committed tester repair:

```text
python.exe -W error::ResourceWarning -m unittest discover -s acceptance -p "test_*.py" -v
exit 1; Ran 2 tests in 16.647s; both failed during setup because /healthz
connection attempts were refused by the incomplete model-authored package
```

No tester repair was used.

## Trace and replay

The required read-only projections succeeded:

```text
rupi.exe trace --config rupi.config.json --session 01a0c04a-d453-733d-9581-280c626a7a47 --tools --sequence --no-color --no-reasoning
exit 0; 63 entries read; 12 tool frames shown
rupi.exe replay .rupi-state/sessions/01a0c04a-d453-733d-9581-280c626a7a47.trace.jsonl --tools --sequence
exit 0; replayed 4 tool requests and 4 completions, including the model write

rupi.exe trace --config rupi.recovery.config.json --session 01a0c04d-093f-71e2-a55c-60a6a76f699d --tools --sequence --no-color --no-reasoning
exit 0; 37 entries read; 6 tool frames shown
rupi.exe replay .rupi-state-recovery/sessions/01a0c04d-093f-71e2-a55c-60a6a76f699d.trace.jsonl --tools --sequence
exit 0; replayed 2 tool requests and 2 completions
```

## Finding

The high implementation stopgate **remains**, but is materially reduced in one
dimension: with the server's reasoning disabled, the model made its first
project write within 40,237 ms. It still failed to complete the package,
README, tests, project suite, or oracle; the next generation timed out and the
provider adapter quarantined its retry. This final environment-only retry is
not an accepted model implementation and used no tester repair.
