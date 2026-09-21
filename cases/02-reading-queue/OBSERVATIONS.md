# Read Queue live-test observations

Date: 2026-09-20
Branch: `tester/2026-09-20-next-live-case`
Draft PR: https://github.com/SaehwanPark/rupi/pull/108
Target: local `qwen3.8-flash-next` at `http://127.0.0.1:8000/v1`

## Selected slice

The selected project is **Read Queue**, a dependency-free Python 3 JSON HTTP
service backed by SQLite. The prior Test Ledger case already covered a local
CLI, JSON-file CRUD, validation, and separate-process checks. Read Queue is the
next small step because it adds a long-lived server, HTTP request/response
boundaries, relational persistence, restart recovery, and integration-test
orchestration without adding third-party dependencies or a UI.

The specification is in `project/SPEC.md`; the runnable implementation is in
`project/readqueue/`; the independent fresh-process oracle is
`acceptance/test_readqueue_http.py`.

## Branch and PR setup

The branch was created from `origin/main` and pushed before implementation.
Creating a draft PR with `gh pr create --draft --head HEAD` before the branch had
a commit failed with GitHub's `No commits between main and HEAD` error. After
the specification/oracle commit `982d884`, the draft PR was created explicitly
for the branch as PR #108. No merge was performed.

## Evidence timeline

### Baseline oracle

Before implementation:

```text
python -m unittest discover -s docs/cases/2026-09-20-reading-queue/acceptance -p "test_*.py" -v
exit 1
server exited 1 ... No module named readqueue
```

This confirmed that the independent check failed for the expected missing
project and was not merely accepting model-written files.

### Initial live implementation run

The implementation prompt was issued through the actual `rupi run` CLI from
`project/`, using `rupi.config.json`, with instructions to build from `SPEC.md`,
use standard-library Python, run the project suite, and stay inside the case
workspace. Session:

```text
01a0bf2e-33cf-7890-9073-9d8b25ff3a6b
turn status: budget_exhausted
model requests: 24
duration: 2,491,586 ms (00:41:31.586)
process exit: 1
```

The trace reported `turn reached the request budget; finalization answer is
incomplete`. The model created most of the package and tests, but did not run
the suite, did not create a README, left a syntax error in `tests/test_http.py`,
and omitted the restart test class.

The model initially tried Unix/shell-shaped commands on Windows:

```text
exec command=ls -la; echo ...       -> 'ls' is not recognized
exec command=cmd /c dir             -> 'dir"' is not recognized
direct process /c dir with cmd.exe  -> succeeded
```

The direct process tool was the reliable recovery path for known executables.

### Initial project failures

After the budget-exhausted run, the project suite was run independently:

```text
python -m unittest discover -s tests -p "test_*.py" -v
exit 1
Ran 67 tests
4 failures, 49 errors
```

The failures were reproducible and localized:

- the HTTP server used an undefined `body_included` name;
- duplicate-URL and unknown-item handlers referenced nonexistent
  `exc.message` attributes, crashing request threads instead of returning JSON
  errors;
- store-level tag encoding did not perform the required trim/lower/unique/sort
  normalization;
- non-list `tags` values such as dictionaries were accepted by validation;
- SQLite test stores were not closed, causing Windows temporary-directory
  cleanup failures (`WinError 32`);
- `test_http.py` had a missing colon and incomplete test scaffolding;
- the restart acceptance scenario and README were missing.

These were project-level defects, not changes to rupi core. The smallest
tester-slice fixes completed the package, closed test resources, added the
restart test and README, corrected the server/error paths, and tightened tag
validation/normalization. The acceptance oracle was also made warning-clean by
closing HTTP errors and subprocess streams.

### Recovery attempt and provider friction

Resuming the exhausted session with `rupi.recovery.config.json` initially used
`thinking: "minimal"` and failed twice with:

```text
provider_unavailable: ------------
process exit: 1
```

The rupi error did not preserve useful provider detail. A direct request to the
same endpoint exposed the cause:

```text
HTTP 500
Unexpected reasoning effort minimal.
Supported types are xhigh (default), medium, and low.
```

Changing the recovery config to `thinking: "low"` allowed a fresh bounded
verification run. The original session was not resumed because the manual
project fixes were already complete and the recovery failure was clearly an
endpoint/configuration mismatch.

### Fresh live verification

The fresh verification prompt was issued through `rupi run` with the recovery
config and told the model not to edit files, to run the project tests through
the direct process tool, and to run both help commands. Session:

```text
01a0bf58-2f36-75a6-8915-0c2a5d2831a5
model requests: 3
turn status: completed
duration: 113,716 ms
process exit: 0
```

The rupi-run process result reported 67 project tests passing and both help
commands exiting 0. It also exposed the same avoidable `ls` attempt before the
model switched to direct process execution.

## Final acceptance evidence

From `project/`:

```text
python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v
Ran 67 tests in 5.467s
OK
exit 0
```

From the case root:

```text
python -W error::ResourceWarning -m unittest discover -s acceptance -p "test_*.py" -v
Ran 1 test in 1.197s
OK
exit 0
```

The independent test starts the service in a fresh process and covers health,
create/list/filter/read, patch, delete, malformed/invalid requests, duplicate
URLs, unknown IDs, and persistence after stopping and restarting against the
same SQLite database. Both `python -m readqueue --help` and
`python -m readqueue serve --help` exited 0. The implementation imports only
Python standard-library modules.

The completed initial trace remained inspectable:

```text
rupi trace 01a0bf2e-33cf-7890-9073-9d8b25ff3a6b --config rupi.recovery.config.json --quiet --no-reasoning
exit 0
rupi replay .rupi-state/sessions/01a0bf2e-33cf-7890-9073-9d8b25ff3a6b.trace.jsonl --tools --sequence
exit 0
```

## Ranked friction and parent actions

1. **High — request-budget exhaustion delayed completion.** The initial coding
   turn spent 41 minutes and all 24 requests before verification, leaving a
   syntactically incomplete project. Parent action: reserve an early test/help
   checkpoint, split multi-file builds into bounded implementation and repair
   turns, and stop planning once the requested slice is clear.
2. **High — endpoint thinking dialect was not validated and provider errors were
   opaque.** Generic `minimal` was accepted by the rupi config surface but
   rejected by this Qwen server; rupi reduced the useful 500 response to blank
   `provider_unavailable` text. Parent action: validate configured thinking
   values against endpoint capabilities or document Qwen's supported
   `xhigh`/`medium`/`low` values, and preserve a bounded provider error detail
   in the user-facing failure.
3. **Medium — Windows tool semantics consumed model requests.** The model
   repeatedly tried `ls` and fragile `cmd /c` quoting before using direct
   process execution. Parent action: make platform/tool invocation guidance
   prominent in the model-visible contract and prefer direct argv execution for
   known programs.
4. **Medium — no self-verification exposed several cross-file defects.** The
   generated server, API exception, validation, resource-lifetime, and test
   files were inconsistent until independently run. Parent action: require a
   first test run before broad cleanup and make the final turn report exact
   command output rather than relying on planned verification.
5. **Low — draft PR creation needs one commit.** GitHub rejected a zero-commit
   branch with `No commits between main and HEAD`; creating the spec commit
   first resolved it. Parent action: document that early draft PR creation is
   performed immediately after the first small evidence/spec commit.

There is no remaining project stopgate: T is complete after the bounded manual
fixes and both live and independent verification paths pass. The initial model
turn itself should still be treated as incomplete in any runtime-quality
assessment.
