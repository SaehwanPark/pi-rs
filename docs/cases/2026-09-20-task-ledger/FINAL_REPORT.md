# Final report: first-user black-box test of `rupi`

Date: 2026-09-20  
Operator: Codex acting as a first-time `rupi` user with general coding-agent
experience  
Host: Windows PowerShell, AMD Ryzen AI Max+ 395  
Model: local llama.cpp, `qwen3.8-flash-next`  
Pull request: [SaehwanPark/rupi#104](https://github.com/SaehwanPark/rupi/pull/104)

## Executive summary

`rupi` was safe and inspectable, but it did not reliably complete a first coding
task in this environment. The first documented-config turn ran for about 24m 41s,
made substantial partial changes, and then failed at the hard 32-model-request
limit without a final answer. A resumed turn preserved the session and added test
files, but after another roughly 25 minutes it still had no final answer and was
stopped while generating. The generated project therefore had a working happy
path but failed its own full test suite and lacked the requested README.

The local model is expected to generate slowly on this machine, so raw latency is
not attributed entirely to `rupi`. The product issues are the combination of that
latency with opaque progress, an undocumented/non-configurable per-turn request
budget, unclear Windows shell semantics, and a resume path that offers no bounded
continuation or completion guarantee.

The strongest positive result is durability: `rupi` recorded native reasoning,
tool lifecycle state, partial files, failures, and the budget-abort diagnostic.
`rupi trace` and `rupi replay` remained usable after both incomplete runs, and the
workspace-confinement check correctly refused an attempted out-of-root write.

## Test subject and acceptance criteria

The toy project was a dependency-free Python 3 command-line task ledger named
`tasklog`. Its complete specification is in
[`toy-project/SPEC.md`](toy-project/SPEC.md). The required interface was:

```text
python -m tasklog add TEXT
python -m tasklog list [--all]
python -m tasklog done ID
python -m tasklog remove ID
```

The project had objective checks:

- `python -m unittest discover -s tests -v` passes;
- separate processes can add, list, complete, and remove tasks with durable JSON
  state;
- invalid input returns non-zero, actionable errors and leaves state unchanged;
- a fresh directory can list without a pre-created state file;
- `--help` documents commands and persistence;
- only the Python standard library is used.

The case intentionally did not test remote providers, MCP, Pi package
installation, extensions, or the interactive TUI. The harness source was not
modified.

## Procedure and results

### 1. Setup

The repository was already on `main` and matched `origin/main`; the unrelated
untracked `.pi/.goals-pool-snapshot.json` was preserved. A test branch was created
and pushed before model work. The documented local endpoint was live and reported
the exact alias `qwen3.8-flash-next`.

The primary case config followed the quickstart shape:

- `primary`: `local/qwen3.8-flash-next`;
- `base_url`: `http://127.0.0.1:8000/v1`;
- `thinking`: `xhigh`;
- context window: 262,144;
- max output: 32,768;
- mutations explicitly enabled only inside the isolated toy workspace.

The configuration is preserved as
[`toy-project/rupi.config.json`](toy-project/rupi.config.json). A separate
`rupi.recovery.config.json` lowered only the configured thinking level for the
resume experiment; it did not change the original setup.

### 2. Initial `rupi run`

The first prompt asked the agent to read the spec, implement the complete project,
write tests, run the suite and smoke test, and return a summary. The command used
the built debug binary from the repository checkout with `--config`, `--cwd .`,
and `--prompt`.

Observed outcome:

- elapsed: 1,480,843 ms (about 24m 41s);
- process exit: 1;
- 32 model requests in one turn;
- generated `tasklog/model.py`, `storage.py`, and `cli.py` plus package files;
- ran some smoke commands and discovered several shell problems;
- did not generate tests or README before the turn ended;
- ended with `turn aborted: model request budget exhausted`.

The canonical trace ended with a warning that the turn stopped after 32 model
requests without a final answer, a `turn_completed` status of
`budget_exhausted`, and a fatal `session_ended` event. The first trace contained
9,917 entries and was about 4.38 MiB. Assistant output and diagnostics were
separated into stdout/stderr as documented, but the stdout contained only partial
progress rather than a final result.

### 3. Trace and replay

The failed session ID was:

```text
01a0bd1d-3e4a-72fd-812b-e67ed5aa2494
```

These commands worked without starting the provider or executing old tools:

```text
rupi trace --config rupi.config.json <session-id> --no-color --no-reasoning
rupi replay .rupi-state/sessions/<session-id>.trace.jsonl --tools --sequence
```

`trace` rendered model requests, tool states, timings, reductions, and the final
budget diagnostic. `replay` produced a compact ordered tool lifecycle projection.
The default trace is very verbose for a multi-thousand-event session; `--quiet` or
`--no-reasoning` is practically necessary.

### 4. Resume experiment

The partial session was resumed with a short prompt asking the agent to add only
tests and README, run the test suite, and avoid exploratory shell workarounds.
The same session ID was appended to rather than copied into a new session.

Observed outcome:

- first resumed request: 124 seconds, with first visible delta after 115 seconds;
- the resumed turn added `tests/support.py`, `test_model.py`,
  `test_storage.py`, and `test_cli.py`;
- it did not create `README.md` or return a final answer;
- after roughly 25 minutes, the client was stopped while a model request was still
  generating; the wrapper exit was `-1`.

After the interrupted recovery, read-only inspection still worked:

- `rupi trace --quiet --no-reasoning` read 20,861 entries and exited 0;
- replay exited 0 and showed the latest completed test-file writes.

The forced client stop is not treated as a normal Ctrl-C result. It is evidence
that an operator may need to interrupt a long recovery, and that inspection can
remain available afterward.

### 5. Independent project validation

The generated project was evaluated without using the model’s claims as evidence.

| Check | Result |
| --- | --- |
| `python -m unittest discover -s tests -v` | **Fail**, 78 tests discovered; 3 failures and 11 errors; exit 1 |
| Fresh-directory `list` | Pass, exit 0, no state file required |
| Two adds, list, done, list-all, remove | Pass across separate Python processes |
| Unknown id | Pass, non-zero exit and actionable stderr |
| Invalid command / missing argument | Pass, non-zero exit and usage text |
| Invalid command leaves JSON unchanged | Pass for tested unknown-id case; SHA-256 unchanged |
| `python -m tasklog --help` | Pass, exit 0 and documents commands/state behavior |
| README deliverable | **Missing** |

The full test-suite errors were primarily an agent-generated internal contract
mismatch: test helpers unpacked `Ledger.mark_done()` as two values while the
implementation returned three. Other generated-test failures covered a Unicode
digit accepted as an ID, a default-list summary that included completed counts,
and a `--state` option passed before the subcommand even though the parser accepts
it only after the subcommand. These are toy-project quality findings; no fixes
were applied after discovery.

## Findings

Detailed chronological evidence is in
[`OBSERVATIONS.md`](OBSERVATIONS.md). The severity summary is:

| ID | Severity | Finding | Primary condition |
| --- | --- | --- | --- |
| F-01 | High | Documented first turn exhausts a hard request budget without final answer | `xhigh`, local Qwen, first coding task |
| F-02 | Medium | Windows shell and quoting behavior is unclear and costly | `exec` commands on Windows/cmd |
| F-05 | High | Resume is durable but not predictably completable | resumed partial session, lower thinking effort |
| F-06 | High | Agent-produced project fails its own verification and lacks README | after both model turns |
| O-03 | Positive | Workspace escape was rejected with a useful resolved-path error | attempted out-of-root write |
| O-04 | Positive | Trace/replay work after fatal budget termination | aborted initial turn |
| O-07 | Positive | Explicit mutation policy and durable tool records work | isolated trusted workspace |

### F-01 — Hard request budget is not discoverable during a long task

The 32-request guard is a sensible protection against an infinite tool loop, but
the first-user experience is poor under a slow local model. The manual presents
`rupi run` as one durable coding turn and does not explain the guard, show a
request counter, provide a user-configurable limit, or suggest what to do when the
agent is making useful progress but reaches the limit. The user sees long waits and
then a failure after partial work.

This is not simply “local inference is slow.” The issue is the interaction between
slow generation, large exposed reasoning, repeated small edits, no live progress
estimate, and a terminal failure that does not itself explain resume strategy.

### F-02 — Windows shell behavior is not first-user discoverable

The model’s first workspace probe used Unix commands and failed because `ls` was
not recognized. After adapting to `cmd.exe`, normal `python -c` commands repeatedly
became `SyntaxError: unterminated string literal`. Compound `%ERRORLEVEL%`
commands also reported misleading zero statuses because of Windows expansion
rules. The model spent multiple requests building workarounds instead of testing
the project.

The manual explains Unix `sh -c` behavior but does not state the Windows shell
contract or give robust PowerShell/cmd examples. Whether the underlying quoting
problem belongs to the model, the `exec` wrapper, or both, it is a material
first-user friction and should be reproducible/documented separately.

### F-05 — Resume is a partial recovery mechanism, not a bounded continuation

`--resume` correctly reopened the existing session and retained the durable trace,
which is valuable. It did not, however, provide a quick path to finish. Lowering
thinking effort in the recovery config did not make the first resumed response
immediate; the first visible delta still took 115 seconds, and the recovery turn
remained incomplete after about 25 minutes.

The manual says how to resume but does not say how to limit a recovery slice,
request a finalization-only pass, inspect the unfinished turn before continuing,
or recognize that a client interruption may leave the latest request without a
clean terminal event.

### F-06 — “Project developed” and “project verified” diverged

The agent generated a respectable layered implementation and a useful smoke path,
but its own tests were internally inconsistent and the requested README was never
created. Because the initial turn ended at the request limit and the recovery turn
was stopped before final verification, the user is left with a directory that
looks substantially complete but fails the explicitly stated acceptance command.

This is exactly the kind of failure a new coding-agent user needs the harness to
surface clearly: partial artifacts are present, but completion status is not
trustworthy without independent checks.

## What worked well

- The documented local endpoint and model alias were accepted.
- The explicit mutation setting was effective in the isolated workspace.
- Reads/writes/edits/execs were represented as durable lifecycle events.
- Native reasoning was retained with explicit provenance in the trace.
- The workspace root was enforced; an attempted traversal write was refused and
  the error named the resolved path.
- The partial session remained inspectable after the budget failure.
- `trace` and `replay` were read-only in practice and did not contact the model or
  rerun recorded commands.
- The task ledger’s happy path and invalid-state preservation worked in an
  independent smoke test, even though the full generated suite failed.

## Recommendations for maintainers

These are observations for future product work, not changes made in this case:

1. Surface request-budget progress and terminal diagnostics in the normal
   transcript. Explain the 32-request guard in the user guide and say exactly how
   to resume partial work.
2. Add a bounded continuation/finalization option for long local-model tasks, or
   expose the request budget in trusted configuration with a clear safety warning.
3. Show a lightweight heartbeat while provider generation is active, including
   whether the process is prefilling, generating, executing a tool, or waiting for
   the next model request.
4. Document the Windows `exec` shell explicitly, or provide a platform-neutral
   command runner that avoids quoting traps for Python and exit-code checks.
5. Make resume report the recovered session state before sending the next provider
   request, and make interrupted/incomplete turns easy to distinguish from clean
   session endings.
6. Include a small first-task recipe that asks the model to implement one thin
   slice, run tests, and stop, rather than relying on a single large prompt under
   maximum reasoning effort.

## Limits of this case

- One local model, one llama.cpp server, one Windows host, and one toy project were
  tested.
- The interactive TUI was not exercised; this was a headless `run`/`trace`/`replay`
  workflow.
- MCP, Pi compatibility commands, remote providers, failover, and extension hosts
  were out of scope.
- The raw model speed was intentionally allowed to be slow; latency observations
  are environment-specific, while the lack of progress/budget guidance is the
  product observation.
- The recovery client was force-stopped after no visible progress. This is not a
  claim that normal Ctrl-C has identical persistence semantics.
- Generated `.rupi-state/`, Python bytecode, and smoke data were excluded from the
  committed case.

## Final verdict

For a new user on this documented local setup, `rupi` is **safe to experiment with
and strong at preserving evidence, but not yet reliable as a first-task coding
harness**. The user can recover partial files and inspect exactly what happened,
but completion requires unusually long, opaque model turns and independent
verification. The case is complete with the toy project intentionally left in its
observed partially verified state.

