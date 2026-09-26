## Round 1 audit

I reviewed the current `main` at merge commit `3d537ba` (PR #119), both benchmark reports, the benchmark harness, Rupi's context/runtime/provider/tool code, and the corresponding Pi 0.86.1 implementation. The benchmark is already doing something valuable: it has exposed several architectural gaps that unit tests alone would not have shown.

My Round-1 conclusion is that **four things should be addressed before spending effort on individual benchmark cases**.

| Priority  | Finding                                                    | Why it matters                                                 |
| --------- | ---------------------------------------------------------- | -------------------------------------------------------------- |
| **P0**    | Pi/Rupi token accounting is currently incomparable         | The reported 40.6× input-token difference is materially biased |
| **P0**    | Rupi cannot compact an active long tool-using turn         | This is a real architectural disadvantage vs Pi                |
| **P0**    | `ReducePayload` is effectively dead in the normal runtime  | The context policy says “reduce” but the runtime does nothing  |
| **P0/P1** | Rupi has essentially no default coding-agent system prompt | Particularly harmful for fragile/quantized local models        |
| **P1**    | Headless budget exhaustion is surfaced as process failure  | Makes autonomous orchestration less graceful than it should be |

I would make those the scope of the next implementation PR rather than trying to fix the Python benchmark cases one at a time.

### 1. The benchmark's 40.6× token headline needs to be corrected first

This is the most important finding about PR #119 itself.

`bench/compare-pi-rupi.ps1` currently measures Pi like this:

```powershell
$input += [int64]$usage.input
$output += [int64]$usage.output
...
total_tokens = $input + $output
```

It completely ignores:

```text
usage.cacheRead
usage.cacheWrite
usage.totalTokens
```

But Pi's OpenAI-compatible provider explicitly converts the server response this way:

```text
input = prompt_tokens - cached_tokens - cache_write_tokens
cacheRead = cached_tokens
totalTokens = input + output + cacheRead + cacheWrite
```

Rupi, by contrast, currently reads:

```rust
usage.prompt_tokens
```

straight into:

```rust
CompletionUsage.input_tokens
```

and has no cache-read field at all.

This matters especially with llama.cpp because its OpenAI-compatible response reports `prompt_tokens_details.cached_tokens`; current llama.cpp also describes cached prompt tokens separately in the response. ([GitHub][1]) Pi's own usage model correspondingly treats `cacheRead` separately from `input`. ([GitHub][2])

So the current comparison is approximately:

```text
Rupi: total logical prompt tokens
Pi:   newly evaluated/noncached prompt tokens
```

That is not comparable.

**Therefore I would withdraw the exact “40.6× input-token amplification” conclusion for now.** The qualitative observation that Rupi carries too much context remains valid—the request histories and runtime implementation independently demonstrate that—but the multiplier needs to be rerun.

Round 1 should add these metrics to both sides:

```text
logical_prompt_tokens
uncached_input_tokens
cache_read_tokens
cache_write_tokens
output_tokens
provider_total_tokens
```

For Rupi that means extending `CompletionUsage` and `decode.rs` to understand `prompt_tokens_details.cached_tokens`. Then the benchmark can report two useful ratios independently:

```text
context carried per request       # logical prompt footprint
tokens actually prefetched/eval'd # inference cost
```

This will be much more useful for local inference optimization than one overloaded `input_tokens` number.

---

## 2. The deeper context bug is intra-turn compaction, not simply resume

PR #119 correctly identified context growth, but the current report frames it too much as a resume/history problem.

The more serious issue is visible directly in `crates/rupi-runtime/src/turn.rs`.

Rupi protects everything at or after `turn_history_start`. Its eviction logic only finds safe boundaries that begin at a new **user turn**:

```rust
if boundary < messages.len() && messages[boundary].role != Role::User {
  return false;
}
```

Consequently, during one coding task such as:

```text
user
assistant -> read
tool result
assistant -> read
tool result
assistant -> exec
tool result
...
```

all 20+ model/tool exchanges belong to one user turn and are effectively indivisible.

That is exactly the workload in Round 2: Rupi is allowed up to **24 model requests inside one benchmark turn**.

Pi does not impose this restriction. Its current compactor explicitly detects a cut point **inside a turn**, creates a summary of the older turn prefix, and retains a recent suffix. Its code even names the condition `isSplitTurn` and has a dedicated turn-prefix summarization path. This is a substantive Pi architectural advantage, not merely tuning. Pi 0.86.1 also defaults to keeping roughly 20k recent tokens during compaction. ([GitHub][3])

### Rupi should adopt the same architectural capability, not necessarily Pi's exact implementation

The canonical trace should remain immutable. Only the **model-visible projection** should change:

```text
Canonical journal
  user task
  assistant/tool cycles 1..17
  assistant/tool cycles 18..22
  ...

Model projection after pressure
  system
  original task / structured checkpoint
  summary of completed cycles 1..17
  raw recent cycles 18..22
```

The safe compaction unit should no longer be “whole user turn.” It should be something closer to a **completed interaction boundary**:

```text
assistant tool-call(s)
        +
all corresponding tool result(s)
```

Never split:

* a tool call from its result;
* an unresolved/in-flight tool lifecycle;
* the current model response;
* the original task/objective from its retained checkpoint.

This is especially important for weak local models. Those models benefit disproportionately from a clean recent context instead of 40–100k tokens of repetitive reads and command outputs.

### There is another problem with Rupi's current summarizer

`structured_summary()` currently reduces history to things like:

```text
- User: <first 120 chars>
- Assistant: <first 120 chars>
- Action: called tool `read`
- Tool: completed execution
```

That is far too lossy for coding work.

It discards exactly the facts needed after compaction:

```text
which files changed
what changes were made
what tests passed/failed
exact remaining errors
important command output
decisions and constraints
unfinished work
```

Rupi already has the beginnings of the right abstraction in `ContextCapsule`. I would build on that rather than copying Pi verbatim.

A coding checkpoint should roughly retain:

```text
objective
constraints
files read
files modified + concise change descriptions
commands/tests run + status
important errors
decisions
unfinished requirements
next actions
```

For fragile models, structured fields are preferable to a loose prose summary.

---

## 3. `ReducePayload` is currently a policy action with no runtime effect

This looks like an outright implementation gap.

`ProfilePolicy` can return:

```rust
ContextAction::ReducePayload { ... }
```

including when:

```rust
state.recent_tokens > thresholds.recent_target_tokens
```

But the main request path in `turn.rs` effectively does:

```rust
ContextAction::Warn { .. }
| ContextAction::Keep
| ContextAction::ReducePayload { .. } => {}
```

So `ReducePayload` means **do nothing**.

Worse, the `ContextState` populated there sets measured/estimated tokens and message count, but does not populate `recent_tokens`. That makes the `RecentTargetExceeded` branch effectively unreachable in this path.

This is particularly unfortunate because the policy itself already expresses a reasonable strategy:

```text
L0 payload reduction
       ↓
ordinary compaction
       ↓
checkpoint
```

The runtime simply does not execute the first stage at the aggregate-context level.

Individual tool output reduction **does work**: `rupi-tools` limits model-visible results to approximately 8 KiB by default and archives full output. Therefore PR #119's wording that Rupi simply keeps “unabridged tool outputs” is no longer quite accurate.

The actual failure mode is:

```text
8 KiB result
+ 8 KiB result
+ 8 KiB result
+ assistant reasoning/tool calls
+ ...
× many requests
```

all accumulating inside one protected turn.

So I would implement these together:

1. compute `recent_tokens`;
2. make `ReducePayload` operational;
3. introduce safe same-turn completed-cycle reduction;
4. make ordinary compaction capable of splitting a long turn;
5. leave the canonical journal and archived payloads untouched.

Doing only #2 without #4 will help somewhat but will not reach Pi parity.

---

## 4. Rupi lacks a default coding-agent behavioral contract

`src/run.rs` contains a surprisingly consequential comment:

```rust
// The skill-control prompt is the whole system prompt rupi speaks today,
// and with_system stays unset when there is nothing to offer.
```

So with no discovered skills, Rupi gives the model **no harness-level system prompt at all**.

Pi does. Even with the benchmark flags:

```text
--no-context-files
--no-extensions
--no-skills
--no-prompt-templates
--no-themes
```

Pi still supplies its normal coding-agent prompt. Its current default identifies itself as a coding assistant, describes the active tools, establishes working-directory context, and provides basic tool/work rules. ([GitHub][3])

Therefore this benchmark asymmetry is real—but I **would not “fix” the benchmark by disabling Pi's prompt**. The objective is product parity. Rupi needs an equivalent native behavioral foundation.

For the local-model use case, I would deliberately make Rupi's version slightly more explicit but still short. It should establish things such as:

* you are an implementation/coding agent operating in the supplied workspace;
* inspect relevant files/specification before editing;
* make concrete changes rather than indefinitely planning;
* use `read`/`grep` for inspection, `edit`/`write`/`append` for changes, and `process`/`exec` for execution;
* prefer `process` with explicit argv when a shell is unnecessary;
* after changes, run the most relevant available verification;
* investigate failures and continue fixing them while budget remains;
* don't claim verification that was not actually run.

I would **not** inject benchmark-specific advice such as:

```text
remember __main__.py
use check_same_thread=False
create subprocess sink fixtures
```

Those would merely train the benchmark.

The interesting Round-2 failures—case 07's SQLite affinity, case 10's missing `__main__.py`, case 06's wrong initial status—should remain things the model discovers through specification reading and verification.

---

## 5. Budget exhaustion is treated inconsistently between interactive and headless use

There is also an autonomy/UX issue worth fixing in this round if the patch remains manageable.

The interactive path correctly treats `BudgetExhausted` as a recoverable state:

```text
model request budget exhausted
→ session remains usable
```

But `rupi run` does this:

```rust
if report.budget_exhausted {
  return Err(TurnError::Aborted(TurnStatus::BudgetExhausted));
}
```

which becomes:

```text
turn aborted: model request budget exhausted
```

and a nonzero process exit.

That explains a substantial portion of the Round-2 `12/40` nonzero Rupi turns.

For a durable/resumable agent harness, I think there should be a clear separation between:

```text
fatal execution failure
provider failure
corrupted state
```

and:

```text
valid partial turn
budget boundary reached
session safely resumable
```

I would preserve the structured `BudgetExhausted` status but avoid making a successfully finalized, safely persisted partial turn indistinguishable from a crash to automation.

For example:

```text
exit 0 + structured turn_status=budget_exhausted
```

or a dedicated documented exit code/status contract could work. The important property is that orchestration can distinguish **“resume me”** from **“execution failed.”**

---

# What I would implement in Round 1

I would keep this PR narrowly architectural:

1. **Correct usage accounting.** Extend Rupi usage with cache read/write; fix `Read-PiMetrics`; report logical-context and inference-work metrics separately.
2. **Implement same-turn context reduction.** Populate `recent_tokens`, make `ReducePayload` real, define safe completed-exchange boundaries, and support split-turn compaction while preserving the immutable journal.
3. **Upgrade checkpoint summaries.** Replace the current first-line-only `structured_summary()` as the main coding compaction representation with a structured coding-work capsule; retain recent raw context.
4. **Add a small permanent coding-agent system prompt.** Always present, independently of skills; skills become an appended section rather than the entire system prompt.
5. **Normalize recoverable headless exhaustion.** Keep durable incomplete state explicit without presenting it as an ordinary fatal process failure.

I would add deterministic tests before rerunning the expensive benchmark. In particular, a synthetic **single user turn with 20–30 tool cycles** should prove that model-visible context reaches a plateau, that a tool call is never separated from its result, that the original objective and modified-file/test state survive compaction, and that resume reconstructs exactly the reduced projection.

Then I would rerun only **01, 02, 04, and 06** initially. They give a useful spread: long implementation/spec adherence, server/subprocess behavior, external-process integration, and near-miss exact semantics. Only once those traces look sane would I pay for all ten cases again.

## What I would *not* fix yet

I would not immediately patch special-case guidance for SQLite, `__main__.py`, subprocess fixtures, or specific acceptance-test conventions. Round 2 shows those failures, but they are downstream symptoms. If Rupi has a better behavioral prompt, bounded working context, effective compaction, and reliable verification loop, we can see which of those failures remain genuinely systematic.

Likewise, I would not tune thresholds based on the reported 5.0M-versus-123k input-token numbers until the cache-accounting bug is corrected.

### Round-1 success criteria

For me, the next benchmark does **not** need Rupi to jump immediately from 0/10 to 10/10. Round 1 is successful if we can establish that:

```text
Pi and Rupi token accounting is genuinely comparable;
Rupi's request context no longer grows monotonically through a long tool loop;
same-turn compaction actually fires;
Rupi retains the information needed to continue after compaction;
headless budget boundaries are graceful;
and Rupi consistently behaves like a coding agent even with no skills installed.
```

After that, the remaining success-rate difference becomes much more diagnostically useful. At that point Round 2 of our audit can concentrate on **tool semantics / model-tool ergonomics / verification behavior** rather than fighting a compromised context pipeline.

The biggest correction to PR #119 is therefore: **its diagnosis points in the right direction, but the exact token multiplier is invalid, and the real context defect is deeper than resume—it is Rupi's inability to reduce completed work inside the active coding turn.** That is the first thing I would fix.

[1]: https://github.com/ggml-org/llama.cpp/blob/master/tools/server/server-task.cpp?utm_source=chatgpt.com "llama.cpp/tools/server/server-task.cpp at master · ggml-org/llama.cpp · GitHub"
[2]: https://github.com/earendil-works/pi/blob/main/packages/coding-agent/examples/extensions/subagent/index.ts?utm_source=chatgpt.com "pi/packages/coding-agent/examples/extensions/subagent/index.ts at main · earendil-works/pi · GitHub"
[3]: https://github.com/earendil-works/pi/blob/main/packages/coding-agent/package.json?utm_source=chatgpt.com "pi/packages/coding-agent/package.json at main · earendil-works/pi · GitHub"
