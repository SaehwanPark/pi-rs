I reviewed `main`, the merged task-ledger case in [PR #104](https://github.com/SaehwanPark/rupi/pull/104), the toy implementation/tests, and the corresponding `rupi` runtime paths. The case is useful and surfaced several legitimate harness problems, but I would revise part of its interpretation before turning the findings into product changes.

The main conclusion is: **the harness-level findings around request budgeting, Windows execution, recovery semantics, and observability are well supported; the “78 tests / 3 failures / 11 errors” result is much less clean as evidence of implementation failure because the generated test suite itself is inconsistent with both the generated code and, in several places, the written specification.**

## 1. Reinterpreting the toy-project failures

The most important finding from reading the tests directly is that the reported 14 unsuccessful tests do not represent 14 tasklog defects.

| Observed result                                                  | What is actually happening                                                                                                | Assessment                                                           |
| ---------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------- |
| 11 test errors                                                   | Several tests unpack `Ledger.mark_done()` into 2 values even though its established API returns `(ledger, task, changed)` | **Generated-test defect**                                            |
| Arabic-Indic `"١"` accepted as ID                                | `str.isdigit()` accepts Unicode digits; the test demands ASCII digits                                                     | **Spec ambiguity**, not clear implementation failure                 |
| Default `list` summary says `2 open, 1 done` instead of `2 open` | Rows correctly contain only open tasks; test imposes a filtered-summary convention                                        | **Spec ambiguity**                                                   |
| `--state PATH` before the subcommand fails                       | Implementation defines `--state` on each subparser; test assumes it is a global option                                    | **Generated-test/API mismatch**, neither placement is specified      |
| README absent                                                    | Recovery prompt requested it, but `SPEC.md` does not include README in its acceptance criteria                            | **Prompt completion failure**, not SPEC failure                      |
| “Separate process” persistence                                   | Test suite calls `tasklog.cli.run()` repeatedly inside one Python process                                                 | **Coverage gap**: the SPEC's process boundary is not actually tested |

The 11 errors are especially diagnostic. `Ledger.mark_done()` consistently returns three values in `tasklog/model.py`, and some tests correctly understand that. But these generated tests still contain stale two-value calls:

```text
test_model.py
  line 126: ledger, _ = ledger.mark_done(1)
  line 137: ledger, _ = ledger.mark_done(2)
  line 167: ledger, _ = ledger.mark_done(1)

test_storage.py
  line 32: ledger, _ = ledger.mark_done(1)
```

Those four locations cascade into exactly the sort of widespread setup/helper errors reported by the case. This is much more indicative of **the model writing tests against an outdated mental version of its own implementation** than of `tasklog` failing its requirements.

There is also a **latent test bug that the current failures conceal**. In `test_storage.py`, `test_write_failure_keeps_previous_content_and_no_temp` eventually does:

```python
save_ledger(self.state, Ledger.empty().add("new"))
```

but `.add()` returns `(Ledger, Task)`, not a `Ledger`. Once the earlier fixture error is corrected, this test will fail before it reaches the mocked `os.replace()` path. The test needs to unpack the new ledger first.

So simply correcting the 11 current errors will not make the suite green.

## 2. The benchmark oracle is currently contaminated

This is the biggest methodological improvement I would make to the task-ledger case.

The same model is effectively responsible for:

1. implementing the software;
2. inventing most of its detailed behavioral contracts;
3. writing the tests that supposedly verify those contracts.

That makes the generated suite useful as a measure of **agent engineering coherence**, but not as an independent acceptance oracle.

For example, the specification says malformed IDs must be rejected, but it never defines whether Unicode decimal numerals count as a valid integer representation. Requiring `"١"` to fail is therefore a perfectly reasonable design choice, but it needs to be placed in `SPEC.md` first. The test cannot retrospectively create that requirement.

Likewise, `list` clearly must show only open tasks by default. Nothing says whether a summary may still report the number of completed tasks. And the documented path override is allowed without specifying whether `--state` comes before or after the subcommand.

I would therefore split future toy-case verification into two distinct layers:

**Frozen acceptance oracle:** written before the model starts, kept outside model-owned paths, ideally read-only during the run. It should test only behavior explicitly required by `SPEC.md`.

**Agent-generated tests:** retained exactly as generated and evaluated separately. Failures here answer questions such as “Did the coding agent maintain an internally consistent test suite?” rather than “Does the product satisfy the benchmark specification?”

That change would make future cases much more scientifically interpretable. A failure could then be classified as:

`SPEC failure` → implementation did not satisfy a frozen requirement.

`Generated-test failure` → implementation and self-generated tests diverged.

`Unspecified behavior probe` → test asserted something the specification intentionally left open.

Those are quite different signals.

### The case is also not fully reproducible from the committed evidence

`CASE_PLAN.md` says the exact prompts and command shapes will be preserved in the observation log. But for the initial run, `OBSERVATIONS.md` says only that the implementation prompt is “recorded in the session trace.”

The `.rupi-state` session was deliberately excluded from the committed case. That means somebody examining the repository later cannot reconstruct the exact initial stimulus from the case directory alone.

I would add something like `PROMPTS.md` containing the exact initial and recovery prompts, plus a compact machine-generated `VALIDATION.txt` or JSON file containing commands, exit codes, and named failing tests. The exhaustive trace can remain excluded.

## 3. The tasklog suite does not test one of the SPEC's most important boundaries

`tests/support.py` calls:

```python
run(list(argv), out, err, base=self.base)
```

directly inside the current interpreter.

That is good for fast CLI/component testing, but the SPEC explicitly requires persistence to work **across separate Python processes**.

Even `EndToEndSmokeTests` is therefore not really end-to-end in the process sense.

I would retain the in-process tests because they are fast and diagnostically useful, but add a small frozen acceptance layer that invokes:

```text
<sys.executable> -m tasklog ...
```

using `subprocess.run()` with a temporary working directory and controlled environment. A handful of such tests is enough:

add → independent list → independent done → independent `list --all` → independent remove; plus one invalid operation followed by a byte comparison.

That directly tests the persistence boundary without turning every unit test into a slow subprocess test.

## 4. How I would repair the toy project itself

Before changing code, make the currently ambiguous contracts explicit in `SPEC.md`. My preferences would be:

**IDs:** define the command grammar as ASCII decimal positive integers. This matches how task IDs are displayed, is predictable across terminals, and avoids surprising equivalences such as Arabic-Indic digits. Then replace the broad `isdigit()` acceptance with an explicit ASCII-decimal check.

**List summaries:** make the summary correspond to the rows being displayed. `list` should end with `N open`; `list --all` can report `N open, M done`. This is easier to understand visually.

**`--state`:** I would make it a genuinely global option:

```text
python -m tasklog --state PATH add ...
```

because the option affects the application rather than an individual command, and the top-level documentation already presents it as an application-wide persistence override. Supporting both positions would be convenient, but is not necessary for this toy.

Then fix the stale `mark_done()` unpacking and the hidden `.add()` tuple bug. That should be done only after the desired public behavior is frozen, so tests are modified to match an intentional contract rather than whatever implementation happens to exist.

There are two additional implementation hardening opportunities that the generated tests largely missed.

First, an **existing empty state file currently means an empty ledger**:

```python
if not text.strip():
  return Ledger.empty()
```

I would reject this as corrupt state instead. A *missing* file clearly means “new ledger”; an *existing but zero-length* file may be evidence of truncation. Treating it as empty allows a later successful `add` to replace that evidence with a brand-new ledger, potentially converting corruption into silent data loss. For a task ledger whose specification emphasizes preserving valid tasks, fail-closed is the safer interpretation.

Second, `Ledger.__post_init__()` does not enforce the full internal invariant that `next_id` must exceed every existing task ID. `from_document()` checks it, but direct programmatic construction can produce inconsistent ledgers. I would centralize that invariant so an invalid `Ledger` cannot exist regardless of how it was constructed.

The storage documentation should also narrow its crash-safety claim. `fsync(temp)` followed by `os.replace()` is good practice and protects against partial file contents and many process-failure scenarios, but the statement that a crash always leaves “either the old file or the new one” is stronger than what is portable under arbitrary filesystem/power-loss conditions. If power-failure durability is a deliberate requirement, POSIX directory metadata syncing should be considered separately.

## 5. The task-ledger case uncovered a real rupi request-budget design problem

This finding is strong.

`crates/rupi-runtime/src/turn.rs` currently defines:

```rust
pub const MAX_MODEL_REQUESTS_PER_TURN: usize = 32;
```

and `TurnLoop` defaults to that value. Interestingly, the runtime already has an internal:

```rust
with_max_requests(...)
```

builder. So the abstraction is already variable internally; it is simply not exposed by `RuntimeConfig` or the CLI. `src/run.rs` constructs the turn loop without overriding it.

That makes the case especially actionable: this is not a deep runtime redesign.

I would **not simply remove the cap**. It is an important runaway-tool-loop and cost guard. Instead, make the policy explicit and observable.

The configuration could gain a typed runtime-limit section such as:

```json
"limits": {
  "max_model_requests_per_turn": 32
}
```

with 32 retained as the default and a reasonable hard ceiling enforced by validation. A one-shot CLI override can be useful for experiments, but the durable config should remain the canonical policy.

More importantly, the user should see the approaching limit. At present `TurnProgress::on_request_started()` receives the model but not the request index or maximum. The default headless transcript intentionally hides routine request lines; `tests/run_cli.rs` even asserts that `[request]` does **not** appear without verbose mode.

That design is reasonable for 2–4 requests. It becomes problematic at request 28 of 32.

I would keep the calm default and surface only meaningful thresholds, for example:

```text
[working] request 16/32 · local/qwen3.8-flash-next
[working] request 24/32 · 8 requests remain
[warning] request 30/32 · turn is nearing its safety limit
```

A TTY can update one status line rather than printing permanent lines. A piped/non-TTY invocation can emit sparse milestone lines to stderr.

There is also a documentation inconsistency here: `book/src/user-guide/cli-runner.md` says stderr contains “Model request notifications,” while the normal CLI deliberately suppresses those unless routine diagnostics/verbose output is enabled. Either the documentation should qualify that or the surface behavior should expose the sparse form above.

## 6. Add a finalization phase instead of dying at request 32

This is the product change I think would produce the largest practical improvement.

At present, request 32 can contain another tool action. Once that completes, rupi may simply reach the budget with no model opportunity to tell the user:

* what was completed;
* what remains;
* whether verification passed;
* how to resume safely.

I would use a **soft tool budget plus a reserved finalization request**.

For example, if the configured total is 32, normal tool-capable operation could stop at 31. The final request would have mutation/execution tools disabled and receive a runtime-generated instruction roughly equivalent to: summarize the current completion state, explicitly identify unfinished verification/deliverables, and give a concise continuation recommendation.

Importantly, that should **not turn the run into a successful run**. The durable status can remain `BudgetExhausted` or a more precise `Incomplete`, and the process should still return nonzero. The point is to transform an opaque abort into a useful incomplete result.

I would complement that with a `--finalize` resume mode: hydrate an incomplete session, disable tools, allow one or two requests, and request only a state assessment. That is considerably more deterministic than asking the user to invent a “please just finish now” prompt while the normal unrestricted agent loop remains active.

## 7. Resume is recoverable internally but recorded as fatal externally

This is a semantic inconsistency worth fixing independently of the toy task.

`TurnError::session_recoverable()` treats everything except a durable sink failure as resumable. Budget exhaustion is therefore explicitly recoverable.

Yet `src/run.rs::close_after_failure()` writes a:

```rust
SessionEndReason::Fatal { ... }
```

for such recoverable failures.

And `SessionEndReason` currently only has:

```text
UserExit
Restart
Fatal
```

So the task-ledger trace says the session ended “fatally,” while `--resume` correctly reopens it.

Those two statements disagree at the domain-model level.

I would add something like `Interrupted` or `RecoverableFailure`, with the cause/status recorded alongside it, and reserve `Fatal` for conditions where safe continuation is impossible, especially a sink/durability failure.

This would also make trace inspection much clearer:

```text
session interrupted · turn budget exhausted · resumable
```

rather than a fatal event followed by a functioning resume.

On resume, before contacting the provider, the headless surface should print one compact recovery record: previous turn status, whether an interrupted tool was found/reconciled, restored model/context epoch, and the fresh per-turn request budget. That makes recovery observable without forcing the user to inspect a 20,000-event trace first.

## 8. Windows `exec` needs a product-level fix, not just better prompting

The case report is correct here.

`crates/rupi-tools/src/exec.rs` explicitly does:

```rust
#[cfg(target_os = "windows")]
fn shell_command(command: &str) -> Command {
  let mut cmd = Command::new("cmd");
  cmd.arg("/C").arg(command);
  cmd
}
```

On non-Windows it uses `sh -c`.

That shell contract is not clearly exposed to either the first-time user or, more importantly, the model. Meanwhile the beginner setup instructions themselves are PowerShell-oriented. A model seeing a Windows environment can therefore very reasonably assume PowerShell syntax, Unix syntax from coding priors, or `cmd.exe`, and spend requests discovering which one rupi actually chose.

At minimum, the `exec` tool description/system context should state the exact platform shell:

```text
Shell: cmd.exe /C
Platform: Windows
```

and the guide should say the same thing.

The more robust architectural improvement is to add a **direct process/argv tool** alongside shell `exec`, for example an operation taking:

```json
{
  "program": "python",
  "args": ["-m", "unittest", "discover", "-s", "tests", "-v"]
}
```

This avoids shell quoting completely for the overwhelmingly common agent workload of running compilers, tests, Python, Git commands, etc. Keep `exec` for cases that genuinely need pipes, redirection, shell expansion, or compound commands.

That is preferable to merely teaching the model more `cmd.exe` quoting tricks.

The existing repository test failure involving `sleep 5` reinforces the point: `tests/tool_lifecycle_events.rs` should not depend on a Unix utility to produce a timeout on Windows. Use a platform-neutral test helper executable or a `cfg(windows)` equivalent. Because the test is about **tool lifecycle semantics**, not shell portability, the fixture should eliminate shell-specific behavior as a variable.

## 9. The beginner default probably should not be `xhigh`

Both the root README and the getting-started guide put a first-time local user on Qwen3.8-Flash-Next with `thinking: "xhigh"`; the llama.cpp example similarly requests `--reasoning-effort xhigh`.

The task-ledger case does not prove that `xhigh` is intrinsically wrong, but it is good evidence that **the beginner default currently optimizes maximum model effort instead of time-to-first-success**.

For a tiny dependency-free CLI, two ~25-minute turns without completion is a poor first-user path even if much of that latency belongs to the local model.

I would run a controlled benchmark of the same frozen task at `low`, `medium`, `high`, and `xhigh`, holding model/server parameters otherwise fixed. Measure acceptance pass rate, model request count, wall time, tool-call count, context consumption, and completion-with-final-answer rate.

Unless `xhigh` materially improves acceptance reliability, I would move the beginner configuration to `medium` or `high` and describe `xhigh` as an opt-in for genuinely difficult reasoning tasks.

## 10. Recommended implementation order

I would tackle this in the following order because it separates test validity from real product changes:

1. **Repair the benchmark design first.** Freeze an external task-ledger acceptance suite, add real subprocess tests, preserve exact prompts, and classify generated tests separately. Re-run the current implementation against this oracle before changing rupi.

2. **Expose and surface the request budget.** Add `limits.max_model_requests_per_turn`, pass `(current, max)` through progress events, show sparse near-limit status, and document the guard. Keep a hard safety ceiling.

3. **Implement graceful budget finalization.** Reserve a no-tools final response and add a constrained `--finalize` continuation mode. Preserve a non-success exit/status when the actual task remains incomplete.

4. **Fix Windows execution ergonomics.** Tell the model and user exactly which shell is active, add quoting regression tests, make the lifecycle timeout fixture platform-neutral, and strongly consider a shell-free argv/process tool.

5. **Fix recovery semantics.** Add a resumable/interrupted session-end reason, reserve `Fatal` for genuinely nonrecoverable state, and print a compact recovered-state line before a resumed provider request.

6. **Then clean up the toy project and beginner docs.** Resolve the three ambiguous tasklog behaviors in `SPEC.md`, correct stale generated tests and the latent atomicity-test bug, reject blank existing state files, strengthen ledger invariants, and benchmark a less aggressive first-user reasoning level.

I searched the current open issues for the request-budget, Windows-shell, task-ledger, resume-budget, 32-request, and `sleep` portability findings and did not find matching open issues. So these appear suitable to split into new implementation issues rather than being duplicates.

### Bottom line

I would **keep PR #104 as a valuable first-user failure case**, but revise the interpretation of F-06. The strongest formulation is not “the generated application has 14 behavioral defects.” It is closer to:

> The agent failed to deliver a verified project before its execution/recovery budget was exhausted; its generated test suite was itself internally inconsistent, and several remaining failures asserted behavior not fixed by the benchmark specification.

That is actually a more useful result for rupi. It points directly at what a harness should improve: bounded autonomous execution, trustworthy completion state, recoverability, platform-aware tooling, and an evaluation structure that can distinguish **model coding failure from test-authoring failure from harness failure**.

The case gives strong evidence for those changes, especially the budget/finalization, shell-contract, and recoverable-session work.
