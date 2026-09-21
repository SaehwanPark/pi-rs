# Parent-fix retry report: Webhook Inbox

Date: 2026-09-20
Case: **Webhook Inbox**
Branch: `tester/2026-09-20-loop-3-webhook-inbox`
Parent config commit: `96ca4b3`

## Scope and isolation

This report records the exact retry requested after `96ca4b3`. No rupi Rust
source, rupi tests, canonical documentation, committed case implementation,
or acceptance oracle was changed.

The retry used a new ignored workspace:

```text
_workspace/live-retries/2026-09-20-webhook-inbox/
```

It was created by copying only these committed inputs into `project/`:

```text
SPEC.md
rupi.config.json
rupi.recovery.config.json
rupi.verify.config.json
.gitignore
```

The unchanged committed `acceptance/` directory was copied beside it so that
the oracle would resolve a copied `project/` if a model-authored result existed.
The retry `project/` initially had no `README.md`, `webhookinbox/`, or `tests/`.
No implementation files were pre-created or deleted. The initial and recovery
rupi state roots were separate and ignored by the copied project rules:
`.rupi-state/` and `.rupi-state-recovery/`.

## Config parse verification

Before either model run, each config was passed to the read-only parser path:

```text
rupi.exe trace --config <config> --session parse-probe --silent --no-color
```

All three commands returned exit `1` with `no session id starts with
'parse-probe'; 0 sessions recorded, newest none`. None returned `invalid
config`, proving that parsing and validation succeeded before a model run.

The parsed retry settings were:

| config | state root | thinking | request limit | request timeout |
| --- | --- | ---: | ---: | ---: |
| `rupi.config.json` | `.rupi-state` | `off` | 8 | 30,000 ms |
| `rupi.recovery.config.json` | `.rupi-state-recovery` | `off` | 3 | 30,000 ms |
| `rupi.verify.config.json` | `.rupi-state-verify` | `low` | 8 | 30,000 ms |

## Exact retry results

The implementation and recovery prompts were unchanged copies of the exact
prompts already recorded in `OBSERVATIONS.md`. The command shapes were also
unchanged:

```text
rupi.exe run --config rupi.config.json --cwd . --prompt <implementation prompt> --no-color --no-reasoning --verbose
rupi.exe run --config rupi.recovery.config.json --cwd . --prompt <recovery prompt> --no-color --no-reasoning --verbose
```

The recovery run was necessary because the initial run produced no project
files. Both processes were allowed to exit on their typed timeout result; no
Ctrl-C was sent.

| retry | config / session | elapsed wall time | model requests started / completed | tool requests / completions / failures | first project write | model package + README | project suite | exact result |
| --- | --- | ---: | ---: | ---: | --- | --- | --- | --- |
| R-03 initial | `rupi.config.json` / `01a0c02b-9f19-74d4-8d96-5e57b53e0297` | 82,234 ms (trace turn 82,168 ms) | 5 / 5; 4 normal tool-call completions, final request timed out | 7 / 6 / 1 | explicit no-write | no | not run; `tests/` absent | exit `1`; `turn_completed` status `failed.kind=timeout`; `session_ended` interrupted/provider timeout |
| R-04 recovery | `rupi.recovery.config.json` / `01a0c02d-836d-749b-ab4f-17801a8dd9a9` | 51,713 ms (trace turn 51,650 ms) | 3 / 3; 2 normal tool-call completions, final request timed out | 2 / 2 / 0 | explicit no-write | no | not run; `tests/` absent | exit `1`; `turn_completed` status `failed.kind=timeout`; `session_ended` interrupted/provider timeout |

After both exits, the retry project still contained only the five copied input
files and the two separately ignored state roots. The model therefore did not
produce the package or README, and there is no model-authored result on which
to run a project suite.

The initial model also attempted the avoidable shell-shaped Python probe from
the trace (`python.exe -c ...` with broken quoting), which failed once. It
read the configs and state directory despite the initial prompt's restrictions.
The recovery model made two `SPEC.md` reads and no failed tool request.

## Oracle, trace, and replay boundary

The unchanged independent oracle was **not run**. The user-specified gate was
not met: there was no model-authored package/README or project suite result,
and running the oracle against the committed tester-repaired implementation
would incorrectly claim a successful retry. The project suite was likewise
not run because no `tests/` directory existed.

The required read-only trace/replay checks did run at the end:

```text
rupi.exe trace --config rupi.config.json --session 01a0c02b-9f19-74d4-8d96-5e57b53e0297 --quiet --no-reasoning --no-color
exit 0; 788 entries read

rupi.exe replay .rupi-state/sessions/01a0c02b-9f19-74d4-8d96-5e57b53e0297.trace.jsonl --tools --sequence
exit 0; replayed 7 tool requests, 6 completions, and 1 failure

rupi.exe trace --config rupi.recovery.config.json --session 01a0c02d-836d-749b-ab4f-17801a8dd9a9 --quiet --no-reasoning --no-color
exit 0; 237 entries read

rupi.exe replay .rupi-state-recovery/sessions/01a0c02d-836d-749b-ab4f-17801a8dd9a9.trace.jsonl --tools --sequence
exit 0; replayed 2 tool requests and 2 completions
```

## Finding and precise retry request

The original high rupi stopgate **remains**. The parent fix reduced the
unproductive exposure from minutes to approximately 82 seconds for the initial
run and 52 seconds for recovery, but it did not produce a first project write;
the functional stopgate is unchanged. This retry is not a successful T result.

The next retry should use the same fresh missing-project workspace, unchanged
prompts, unchanged oracle, and these parsed configs only after rupi provides a
reliable progress/first-write boundary that causes the model to create the
package and README before the deadline. Acceptance should require the
model-authored project suite and the unchanged oracle to pass, with no tester
repair, followed by trace/replay verification.
