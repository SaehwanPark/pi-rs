# Final progress-boundary retry report: Webhook Inbox

Date: 2026-09-20
Case: **Webhook Inbox**
Branch: `tester/2026-09-20-loop-3-webhook-inbox`
Parent runtime fix: `e2e00f3` on draft PR #111

## Scope and fresh workspace

The retry used the exact initial and recovery prompt blocks already recorded in
`OBSERVATIONS.md`, with no prompt, oracle, or rupi source changes by the tester.
Rupi was rebuilt from `e2e00f3` before execution.

The model cwd was a new ignored workspace:

```text
_workspace/live-retries/2026-09-20-webhook-inbox-progress/project/
```

It was created from only the committed `SPEC.md`, three configs, and
`.gitignore`. An unchanged copy of the three acceptance oracle/sink files was
placed in the sibling `acceptance/` directory. The prior retry workspace and
committed tester-repaired implementation were not copied or visible in this
project cwd. `README.md`, `webhookinbox/`, and `tests/` were absent before the
initial run and remained absent afterward. No implementation file was
pre-created or deleted. Initial and recovery state roots were separate:
`.rupi-state/` and `.rupi-state-recovery/`, both ignored.

## Config parse

Before running the model, each config used the read-only parser path:

```text
rupi.exe trace --config <config> --session parse-probe --silent --no-color
```

All three commands exited `1` with `no session id starts with 'parse-probe'; 0
sessions recorded, newest none`. None returned `invalid config`, so parse and
validation succeeded. The relevant parsed settings were:

| config | state root | thinking | request limit | no-progress limit | progress tools | request timeout |
| --- | --- | --- | ---: | ---: | --- | ---: |
| `rupi.config.json` | `.rupi-state` | `off` | 8 | 1 | `write, edit, append` | 30,000 ms |
| `rupi.recovery.config.json` | `.rupi-state-recovery` | `off` | 3 | 1 | `write, edit, append` | 30,000 ms |
| `rupi.verify.config.json` | `.rupi-state-verify` | `low` | 8 | unset | unset | 30,000 ms |

## Exact retry results

The command shapes were unchanged:

```text
rupi.exe run --config rupi.config.json --cwd . --prompt <implementation prompt> --no-color --no-reasoning --verbose
rupi.exe run --config rupi.recovery.config.json --cwd . --prompt <recovery prompt> --no-color --no-reasoning --verbose
```

No Ctrl-C was sent. Both processes reached their typed timeout and exited
naturally.

| retry | session / config | elapsed wall time | first project write | model requests started / completed | tool requests / completions / failures | schemas before -> after boundary | model artifacts / project suite | exact exit/status |
| --- | --- | ---: | --- | ---: | ---: | ---: | --- | --- |
| R-05 initial | `01a0c03c-0168-7e3d-901b-4bff5ceac0dd` / `rupi.config.json` | 37,121 ms (trace turn 37,045 ms) | explicit no-write | 2 / 2; first normal tool-call response, second timeout completion | 2 / 2 / 0 | 7 -> 3 | no `webhookinbox/`, `README.md`, or `tests/`; suite not run | exit `1`; `turn_completed` `failed.kind=timeout`; `session_ended` provider timeout |
| R-06 recovery | `01a0c03c-d318-7228-a8af-eace6d50c489` / `rupi.recovery.config.json` | 38,077 ms (trace turn 38,013 ms) | explicit no-write | 2 / 2; first normal tool-call response, second timeout completion | 2 / 2 / 0 | 7 -> 3 | no `webhookinbox/`, `README.md`, or `tests/`; suite not run | exit `1`; `turn_completed` `failed.kind=timeout`; `session_ended` provider timeout |

The initial process started at `2026-09-20T19:12:33.0641005Z` and finished at
`2026-09-20T19:13:10.1909074Z`. Recovery started at
`2026-09-20T19:13:26.7559397Z` and finished at
`2026-09-20T19:14:04.8384277Z`.

## Boundary evidence

For R-05, trace `model_request_started` sequence `4` exposed 7 schemas. After
the first request made only a read and an `exec`, the runtime appended the
nudge at sequence `26`:

```text
Runtime progress boundary: this implementation turn has spent the configured inspection budget without calling a progress tool. In your next response, call one of write, edit, append to make the requested change. Do not spend another request reading, probing, or planning; the turn remains incomplete until the change is attempted.
```

The info diagnostic at sequence `27` said:

```text
progress boundary active after 1 model request(s) without a configured progress tool; next request exposes write, edit, append
```

The next `model_request_started` at sequence `28` exposed 3 schemas. No
`write`, `edit`, or `append` request occurred before the provider timeout.

R-06 has the same evidence: 7 schemas at request sequence `4`, the nudge at
sequence `102`, the info diagnostic at `103`, and 3 schemas at request
sequence `104`; its only tool requests were `read` and `exec`, followed by the
same timeout.

## Oracle, project suite, trace, and replay

The unchanged independent oracle was **not run** because no model-authored
implementation existed. The project suite was also not run because `tests/`
was absent. Running either against the prior tester repair would invalidate
this retry.

The required read-only trace/replay checks ran for both sessions:

```text
rupi.exe trace --config rupi.config.json --session 01a0c03c-0168-7e3d-901b-4bff5ceac0dd --tools --sequence --no-color --no-reasoning
exit 0; 214 entries read; 6 tool frames shown
rupi.exe replay .rupi-state/sessions/01a0c03c-0168-7e3d-901b-4bff5ceac0dd.trace.jsonl --tools --sequence
exit 0; replayed 2 tool requests and 2 completions

rupi.exe trace --config rupi.recovery.config.json --session 01a0c03c-d318-7228-a8af-eace6d50c489 --tools --sequence --no-color --no-reasoning
exit 0; 473 entries read; 6 tool frames shown
rupi.exe replay .rupi-state-recovery/sessions/01a0c03c-d318-7228-a8af-eace6d50c489.trace.jsonl --tools --sequence
exit 0; replayed 2 tool requests and 2 completions
```

## Finding and precise retry request

The high implementation stopgate **remains**, but is operationally reduced.
The new boundary works: it emits durable model-visible guidance and narrows
the next request from 7 schemas to the configured 3 progress schemas. It does
not resolve the stopgate because the model/provider still spends the entire
30-second request without making a progress-tool call, leaving T unimplemented.

The next retry requires a model/provider execution that completes the narrowed
write request within the request deadline, or a parent runtime change that
enforces actual first-write progress rather than only schema narrowing. Keep
the same fresh missing-project setup, prompts, and unchanged oracle; accept
only a model-authored package, README, passing project suite, and passing
oracle, followed by trace/replay.
