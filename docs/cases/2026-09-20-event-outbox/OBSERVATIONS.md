# Event Outbox live-case observations

Date: 2026-09-20
Operator: Codex acting as a fresh `rupi` user
Host: Windows PowerShell, Python 3.14.7
Branch: `tester/2026-09-20-loop-2-live-case`
Base: clean `origin/main` at `7c3e9b3`
Endpoint: `http://127.0.0.1:8000/v1`, model `qwen3.8-flash-next`
Draft PR: https://github.com/SaehwanPark/rupi/pull/110

## Selected project

T is **Event Outbox**, a dependency-free Python HTTP/SQLite service plus a
separate bounded worker. The worker starts a sink with a direct argv list and
exchanges one newline-delimited JSON request/response per pending event. The
service adds idempotent admission, persisted delivery attempts, retry state, and
restart recovery to the HTTP/process/persistence boundary already exercised by
Read Queue.

The specification is `project/SPEC.md`. The independent oracle is
`acceptance/test_event_outbox.py`; its `fake_sink.py` is separate from the
project implementation and is invoked as another fresh process.

## Branch and first PR setup

The branch was created from clean `origin/main` and the first specification/
oracle commit was pushed before implementation:

```text
git switch -c tester/2026-09-20-loop-2-live-case origin/main
git add docs/cases/2026-09-20-event-outbox
git commit -m "test: define Event Outbox live case"
# c8ff2cb
git push -u origin tester/2026-09-20-loop-2-live-case
gh pr create --repo SaehwanPark/rupi --base main --head tester/2026-09-20-loop-2-live-case --draft --title "test: live case 2 Event Outbox"
```

GitHub accepted the draft as PR #110. No merge was performed.

## Baseline oracle

Before implementation, the exact independent command was:

```text
python -m unittest discover -s docs/cases/2026-09-20-event-outbox/acceptance -p "test_*.py" -v
```

Result: exit `1`, wrapper elapsed `4,350 ms`, two tests failed during setup
because the fresh server process reported `No module named outbox`. This was
the expected missing-project baseline and confirmed the oracle was not merely
accepting pre-existing implementation files.

## Live `rupi` implementation attempts

All runs used the checked-out binary
`C:\Users\saehwan\repos\pi-rs\target\debug\rupi.exe` from the project
directory. The endpoint was reachable and `/v1/models` returned the configured
Qwen alias before the runs.

### R-01 — Initial bounded implementation turn interrupted

Config: `project/rupi.config.json` (`thinking: low`, 24-request limit,
32,768 output-token endpoint cap). Exact command shape:

```text
rupi.exe run --config rupi.config.json --cwd . --prompt <implementation prompt> --no-color --no-reasoning --verbose
```

Prompt:

```text
Read SPEC.md and build the complete Event Outbox project in this workspace.

This is one bounded implementation slice. Work only under the current project workspace; do not edit SPEC.md, rupi.config.json, the case acceptance oracle, or files outside this workspace. Use only Python's standard library. Create the outbox package, a readable README.md, and focused tests. Implement the exact HTTP contract, SQLite persistence, idempotent event admission, and the worker's direct-argv newline-delimited JSON sink protocol from SPEC.md. Keep the worker bounded by --once and make retry state explicit.

On this Windows host, use the direct process tool for known programs such as python, with an argv list. Do not spend requests on Unix ls or fragile shell quoting. Run the project test suite early enough to catch cross-file errors, then run it again at the end:
python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v
Do not run or modify the independent acceptance oracle; it is outside this workspace. Do not claim completion from planned tests: run the commands and report their exact exit statuses. Stop after the bounded project is implemented, tested, and documented.
```

Session: `01a0bf89-b2f2-7196-a328-a8619e66a421`. Local trace wall time was
`11:57:47.640` to `12:24:18.249`, or `1,590,609 ms` (`26m 30.609s`). The
terminal was interrupted with Ctrl-C; the wrapper exited `1` and did not reach
its final timing line.

Trace counts: 8 model requests started, 7 completed, 11 tool calls, 3 failed
tool calls, and no `turn_completed` or `session_ended` event. Completed request
durations were `16,126`, `21,368`, `1,154,954`, `18,512`, `11,871`, `30,135`,
and `11,186 ms`. The 19-minute request ended with two Windows `python -c`
quoting failures:

```text
SyntaxError: unterminated string literal (detected at line 1)
```

The model then wrote `_probe.py`, used the direct process tool, and made a
probe fail with `AttributeError: type object 'Popen' has no attribute 'close'`.
It never created `outbox/`, tests, or README. The generated probe was removed
from the project workspace after the run; the trace remains in `.rupi-state/`.

### R-02 — Recovery config reached output length without implementation

The recovery config lowered the endpoint cap to 8,192 output tokens and the
turn limit to eight requests. Exact command shape:

```text
rupi.exe run --config rupi.recovery.config.json --cwd . --prompt <recovery implementation prompt> --no-color --no-reasoning --verbose
```

Prompt:

```text
Implement the Event Outbox project now from SPEC.md. Work only in this project workspace. Start writing the minimal complete outbox package, README.md, and focused tests; do not spend time inspecting Python stdlib internals or making probe files. Use only standard library. Implement the exact HTTP, SQLite, idempotency, retry state, worker direct-argv, and NDJSON sink contracts in SPEC.md.

This is a bounded recovery slice after an interrupted exploratory turn. Do not use shell commands for Python or directory inspection on Windows: use the direct process tool with program python and argv, and use write/edit for files. Do not edit SPEC.md, either rupi config, or the external acceptance oracle. Run the project unittest command once after the implementation exists:
python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v
If a test failure appears, fix only project defects needed by SPEC.md, rerun it, and then stop with exact results. Do not claim success from a planned command.
```

Session: `01a0bfa2-cb37-7a45-a783-06c785674143`. Wrapper elapsed
`439,635 ms` (`7m 19.635s`), exit `0`. Trace counts were 4 requests started and
completed, 5 tool calls, 1 failed read, `turn_completed=completed`, and
`session_ended=user_exit`. The failed read was the attempted
`../acceptance/test_event_outbox.py`, correctly refused as outside the workspace
root. The last model request took `399,962 ms` and ended with
`finish_reason=length`, `output_tokens=8192`, and no tool calls. No project
implementation files were created despite the successful process exit and
completed turn status.

### R-03 — Fast/off-thinking implementation attempt interrupted

The third config used `thinking: off`, a 16,384 output-token cap, and a
12-request limit:

```text
rupi.exe run --config rupi.fast.config.json --cwd . --prompt <short implementation prompt> --no-color --no-reasoning --verbose
```

Prompt:

```text
Build the complete project in SPEC.md immediately. Work only in this workspace and use only Python standard library. Create outbox/, README.md, and tests. Implement the exact HTTP event contract, SQLite durability, idempotent POST behavior, and the bounded worker that spawns the sink with direct argv and exchanges one JSON line per event. Do not inspect anything outside this workspace, do not create probe files, and do not edit SPEC.md or either rupi config. Use write/edit for source files. On Windows use the process tool with program python and argv for commands, not shell quoting or Unix commands. Run the specified project unittest command once after code exists and fix only failures required by SPEC.md. Report exact command status before stopping.
```

The session was
`01a0bfaa-2353-7235-bf75-1c168dea64a1`; local trace wall time was
`12:33:13.561` to `12:42:38.124`, or `564,563 ms` (`9m 24.563s`). Ctrl-C
ended the terminal with exit `1` after 4 requests started, 3 completed, 6
successful tool calls, and no implementation files. The model still spent its
first calls on `dir`/workspace inspection despite the direct-process instruction.

### R-04 — Read-only verification initially lacked approval

The first verification config set `auto_approve_mutating` to `false`. The
read-only prompt asked for the project suite and three help commands using
direct `process`. Session `01a0bfb6-a63f-70e2-b261-f4527c1c7089` ran for
`22,212 ms`, was stopped with Ctrl-C, and had 2 requests started, 1 completed,
and 2 failed tool calls. Both failures were policy diagnostics:

```text
'exec' is mutating and approval is required
'process' is mutating and approval is required
```

The config was corrected to `auto_approve_mutating: true` for this disposable
trusted workspace. The prompt remained explicitly read-only, and the model was
not allowed to edit files.

### R-05 — Final bounded rupi verification passed

Exact command:

```text
rupi.exe run --config rupi.verify.config.json --cwd . --prompt "Perform a bounded read-only verification of the completed Event Outbox project. Do not edit or write any file and do not inspect the case acceptance directory. Use the direct process tool with program python and argv, not shell syntax. Run exactly these checks from the current workspace: 1) python -W error::ResourceWarning -m unittest discover -s tests -p \"test_*.py\" -v 2) python -m outbox --help 3) python -m outbox serve --help 4) python -m outbox worker --help. Report each exact exit status and the observed test summary. Stop after these bounded checks." --no-color --no-reasoning --verbose
```

Session: `01a0bfb7-6897-72c8-a50f-c08b99651c14`. Wrapper elapsed
`78,687 ms` (`1m 18.687s`), exit `0`; trace wall duration was `78,642 ms`.
There were 2 requests, 4 direct `process` calls, 0 failed tool calls,
`turn_completed=completed`, and `session_ended=user_exit`. The model reported:

```text
Ran 6 tests in 0.460s
OK
```

All four process commands returned exit `0`. The rupi trace records the same
four calls and no writes.

## Independent implementation and oracle verification

Because the bounded implementation turns did not produce project files, the
tester wrote the smallest project-local implementation required by the already
committed specification. No rupi source, Rust crate, canonical document, or
roadmap file was changed. The implementation has seven small package modules,
six focused project tests, a README, and three configs preserving the exact
live-run shapes.

The first project check after implementation was:

```text
python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v
```

It passed: 6 tests, exit `0`, wrapper elapsed `577 ms`.

The first oracle run after implementation exposed an oracle command-construction
problem, not a project failure. Passing `--sink-arg --log` to `argparse` made
`--log` look like an outbox option. The oracle was corrected to use the
unambiguous `--sink-arg=--log` form. The next run exposed an oracle sequencing
mistake: both events were admitted before the first worker run, so the planned
failure run had no pending event. The oracle was reordered to deliver `evt-1`
before admitting `evt-2`. These two failures were retained as tester friction;
the spec and final README now show the correct direct-argv form.

The final exact commands and results were:

```text
python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v
Ran 6 tests in 0.449s
OK
exit=0; wrapper elapsed_ms=561

python -W error::ResourceWarning -m unittest discover -s docs/cases/2026-09-20-event-outbox/acceptance -p "test_*.py" -v
Ran 2 tests in 2.644s
OK
exit=0; wrapper elapsed_ms=2780
```

The independent oracle started the server and fake sink in fresh processes,
checked idempotency and conflicting duplicates, verified a delivered event,
forced one failed delivery and a persisted retry, checked malformed input and a
405 JSON error, stopped and restarted the server against the same SQLite file,
and verified the three help commands. A source import review found only Python
standard-library modules in the project and oracle.

## Trace and replay evidence

The incomplete initial session remained inspectable:

```text
rupi.exe trace --config rupi.config.json 01a0bf89-b2f2-7196-a328-a8619e66a421 --quiet --no-reasoning --no-color
exit=0; elapsed_ms=428; 28,909 entries read; state failed; 3 entries shown

rupi.exe replay .rupi-state/sessions/01a0bf89-b2f2-7196-a328-a8619e66a421.trace.jsonl --tools --sequence
exit=0; elapsed_ms=672
```

Replay showed the failed shell probes, the refused outside-workspace read, and
the direct process/write lifecycle without executing any historical tool.

The completed verification session was also read and replayed:

```text
rupi.exe trace --config rupi.verify.config.json 01a0bfb7-6897-72c8-a50f-c08b99651c14 --no-reasoning --no-color
exit=0; elapsed_ms=34; 1,174 entries read; 23 displayed

rupi.exe replay .rupi-state/sessions/01a0bfb7-6897-72c8-a50f-c08b99651c14.trace.jsonl --tools --sequence
exit=0; elapsed_ms=51
```

The final replay showed four successful direct `process` lifecycles.

## Ranked friction

1. **High — local model turns spent most of their bounded window reasoning or
   inspecting before useful implementation.** The initial run consumed 26m 30s
   and 8 request starts without writing the project; the recovery run consumed
   7m 20s and reached an output-length finish with no implementation. This is
   partly local-model latency, but the combined request/time progress is opaque
   to a first user.
2. **High — a length-limited turn can appear completed to the caller without
   completing the requested work.** R-02 exited zero and recorded
   `turn_completed=completed` even though its final model request ended with
   `finish_reason=length` and no files had been produced. A completion status
   that distinguishes a useful final answer from an output-boundary stop would
   reduce false confidence.
3. **Medium — Windows shell/tool semantics still consumed model requests.** The
   model used `dir` and shell-shaped `python -c` commands despite direct-argv
   guidance; two quoting failures and a failed probe preceded any productive
   action. The workspace boundary did correctly refuse the attempted external
   oracle read.
4. **Medium — mutation approval applies to read-only verification process
   calls.** The first verification config blocked both `exec` and `process` until
   the trusted disposable workspace explicitly enabled auto-approval. The
   distinction is safe, but the setup is easy to misread when a user only wants
   tests.
5. **Low — option-like direct argv values require `--sink-arg=...`.** The
   independent oracle initially passed `--sink-arg --log` and failed before
   worker execution. The documentation and oracle now use the unambiguous form.

## Residual rupi issue

The Event Outbox project is complete and independently accepted. A major rupi
issue remains for this environment: a local Qwen turn can spend many minutes and
several requests before its first useful write, and an output-length boundary can
be surfaced as a successful completed turn. This is recorded as a reproducible
runtime/UX issue, not papered over with a Rust change. The smallest reproduction
is R-02: an 8,192-token endpoint cap, four requests, the final
`finish_reason=length`, process exit `0`, and no project files. No project
acceptance claim relies on model prose.
