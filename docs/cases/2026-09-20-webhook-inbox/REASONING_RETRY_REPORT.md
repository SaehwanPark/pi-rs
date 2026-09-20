# Final provider-effort retry report: Webhook Inbox

Date: 2026-09-20
Case: **Webhook Inbox**
Branch: `tester/2026-09-20-loop-3-webhook-inbox`
Parent config commit: `fc56ac4`

## Scope and isolation

This was the final bounded retry for the case. Rupi was rebuilt from the
current branch head `fc56ac4` with:

```text
cargo build --bin rupi
```

The exact initial and recovery prompt blocks and command shapes from
`OBSERVATIONS.md` were reused without modification. No prompt, oracle, rupi
Rust source, or canonical documentation was changed by the tester.

The model cwd was a brand-new ignored workspace:

```text
_workspace/live-retries/2026-09-20-webhook-inbox-reasoning/project/
```

It contained only copied `SPEC.md`, the three case configs, and `.gitignore`.
The three unchanged oracle/sink files were copied into its sibling
`acceptance/` directory. The prior tester repair and both earlier retry
workspaces were not copied into the model cwd. `README.md`, `webhookinbox/`,
and `tests/` were absent before and after both runs. No implementation file
was pre-created or deleted. Initial and recovery state roots were separate and
ignored: `.rupi-state/` and `.rupi-state-recovery/`.

## Config parse

Each config was parsed before model execution through:

```text
rupi.exe trace --config <config> --session parse-probe --silent --no-color
```

All three commands exited `1` with `no session id starts with 'parse-probe'; 0
sessions recorded, newest none`. None returned `invalid config`.

| config | thinking | request timeout | request limit | no-progress limit | progress tools |
| --- | --- | ---: | ---: | ---: | --- |
| `rupi.config.json` | `low` | 60,000 ms | 8 | 1 | `write, edit, append` |
| `rupi.recovery.config.json` | `low` | 60,000 ms | 3 | 1 | `write, edit, append` |
| `rupi.verify.config.json` | `low` | 30,000 ms | 8 | unset | unset |

## Retry results

Both processes were allowed to reach their typed provider deadlines naturally;
no Ctrl-C was sent.

| retry | session / config | elapsed wall time | first project write | model requests started / completed | tool requests / completions / failures | schemas before -> after boundary | model files / project suite | exact exit/status |
| --- | --- | ---: | --- | ---: | ---: | ---: | --- | --- |
| R-07 initial | `01a0c042-e535-7ea5-87ca-0912a63b76ea` / `rupi.config.json` | 66,666 ms (trace turn 66,601 ms) | explicit no-write | 2 / 2; first normal tool-call response, second timeout completion | 2 / 2 / 0 | 7 -> 3 | no `webhookinbox/`, `README.md`, or `tests/`; suite not run | exit `1`; `turn_completed` `failed.kind=timeout`; `session_ended` provider timeout |
| R-08 recovery | `01a0c044-2e31-7a68-a005-c18ca45f33cd` / `rupi.recovery.config.json` | 70,365 ms (trace turn 70,291 ms) | explicit no-write | 2 / 2; first normal tool-call response, second timeout completion | 1 / 1 / 0 | 7 -> 3 | no `webhookinbox/`, `README.md`, or `tests/`; suite not run | exit `1`; `turn_completed` `failed.kind=timeout`; `session_ended` provider timeout |

The initial process ran from `2026-09-20T19:20:04.6103594Z` to
`2026-09-20T19:21:11.2821549Z`. Recovery ran from
`2026-09-20T19:21:28.8249937Z` to `2026-09-20T19:22:39.1956887Z`.

## Schema, nudge, and diagnostic evidence

R-07 exposed 7 schemas at `model_request_started` sequence `4`. Its first
request made only `read(SPEC.md)` and `exec(dir)`. The runtime appended the
following model-visible nudge at sequence `28` and emitted the info diagnostic
at sequence `29`:

```text
Runtime progress boundary: this implementation turn has spent the configured inspection budget without calling a progress tool. In your next response, call one of write, edit, append to make the requested change. Do not spend another request reading, probing, or planning; the turn remains incomplete until the change is attempted.

progress boundary active after 1 model request(s) without a configured progress tool; next request exposes write, edit, append
```

The next request at sequence `30` exposed 3 schemas. No `write`, `edit`, or
`append` request occurred before the 60-second provider timeout.

R-08 has the same boundary transition: 7 schemas at request sequence `4`,
only `read(SPEC.md)` before the boundary, nudge at sequence `120`, diagnostic
at `121`, and 3 schemas at request sequence `122`. It also made no progress-tool
request before timeout.

The explicit `thinking: low` setting therefore changed the configured request
effort from the prior inherited server default, but did not produce a first
project write in this live case.

## Oracle, project suite, trace, and replay

The project suite and unchanged independent oracle were **not run**. No
model-authored implementation existed, so running either against the committed
tester repair would falsely claim success for this retry.

The required read-only trace/replay checks ran for both sessions:

```text
rupi.exe trace --config rupi.config.json --session 01a0c042-e535-7ea5-87ca-0912a63b76ea --tools --sequence --no-color --no-reasoning
exit 0; 892 entries read; 6 tool frames shown
rupi.exe replay .rupi-state/sessions/01a0c042-e535-7ea5-87ca-0912a63b76ea.trace.jsonl --tools --sequence
exit 0; replayed 2 tool requests and 2 completions

rupi.exe trace --config rupi.recovery.config.json --session 01a0c044-2e31-7a68-a005-c18ca45f33cd --tools --sequence --no-color --no-reasoning
exit 0; 1,162 entries read; 3 tool frames shown
rupi.exe replay .rupi-state-recovery/sessions/01a0c044-2e31-7a68-a005-c18ca45f33cd.trace.jsonl --tools --sequence
exit 0; replayed 1 tool request and 1 completion
```

## Finding

The high implementation stopgate **remains**. Explicit provider effort
(`thinking: low`), a 60-second request deadline, and the progress boundary all
worked as configured: the boundary narrowed the request from 7 schemas to the
three progress tools and emitted durable guidance. The model nevertheless
spent the narrowed request at the provider deadline without calling a progress
tool. No model-authored project result exists, and no tester repair was used.

This is the final bounded case retry; the case remains a rupi stopgate rather
than an accepted model implementation.
