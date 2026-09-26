## Round 2 audit

I re-audited current `main` at `510516d` after PR #120 and compared the relevant paths against current Pi. PR #120 materially improved Rupi: the Round-1 token accounting problem is fixed, same-turn compaction now exists, `ReducePayload` is active, the coding-agent prompt is present, and budget exhaustion is resumable.

I would **not run the full 10-case benchmark again yet**. Round 2 uncovered three P0 issues and one P0/P1 issue that can still distort real agent behavior, particularly with fragile local models.

| Priority  | Finding                                                               | Impact                                                                                     |
| --------- | --------------------------------------------------------------------- | ------------------------------------------------------------------------------------------ |
| **P0**    | Same-turn compaction can cross an older `Unknown` tool lifecycle      | Can hide uncertain side effects inside a summary                                           |
| **P0**    | Tool exposure and approval are inconsistent                           | Default Rupi advertises actions it will refuse; interactive approval is not actually wired |
| **P0**    | Context policy remains sized to the primary model after failover      | A smaller backup can regrow beyond its appropriate working budget                          |
| **P0/P1** | Malformed local-model tool calls are treated as fatal protocol errors | Fragile/quantized models lose the turn instead of getting a chance to self-correct         |
| **P1**    | Context policy estimates omit system prompt + tool schemas            | Especially problematic for small windows and MCP-heavy sessions                            |
| **P1**    | Budget-exhausted one-shot sessions are closed as `UserExit`           | Durable state says the user finished when work is actually incomplete                      |
| **P1**    | Cache accounting still misses some OpenAI-compatible dialects         | Weaker provider compatibility than Pi                                                      |

### 1. P0: the new same-turn compactor has an uncertainty-boundary bug

The architectural addition in PR #120 is the right one. `compact_completed_turn_cycles()` now allows Rupi to fold completed work within a single long user turn, which fixes the major Round-1 limitation.

But `safe_completed_cycle_boundary()` does not actually validate the entire prefix it allows the runtime to compact.

It walks backward and returns `true` immediately after matching the most recent assistant tool-call group with its result:

```rust
Role::Assistant => {
  let calls: Vec<_> = message.tool_calls().collect();
  ...
  if calls.iter().any(|call| !results.remove(&call.id)) {
    return false;
  }
  return results.is_empty();
}
```

Consider:

```text
user
assistant -> mutating call A
tool A -> Unknown

assistant -> read/call B
tool B -> Succeeded
                 ↑ proposed compaction boundary
```

Walking backward validates B and immediately returns `true`. It never reaches A.

`compact_completed_turn_cycles()` then summarizes:

```rust
self.messages[protected..boundary]
```

which includes the uncertain A lifecycle.

That directly contradicts the function's own intended invariant:

> Unknown results are kept in the visible window so a later request cannot mistake an uncertain mutation for a completed cycle.

This one deserves an immediate fix because Rupi has been deliberately careful elsewhere not to convert “we don't know whether the side effect happened” into ordinary completed history.

The safe rule should be stronger: **a compaction boundary may not cross any unresolved or `Unknown` lifecycle**. Either validate every tool lifecycle in the whole candidate prefix, or identify the newest unsafe lifecycle and forbid any boundary after it until it has been reconciled.

A regression test should explicitly use:

```text
successful cycle
Unknown mutating cycle
successful cycle
```

and prove compaction never crosses the `Unknown` cycle. The current tests cover an `Unknown` result at the candidate's latest interaction, but not one earlier in the prefix.

---

### 2. P0: Rupi's model-visible tool contract does not match what it can execute

This is probably the largest capability gap not exercised by PRs #118/#119.

The default configuration has:

```rust
auto_approve_mutating = false
```

which is a sensible safe default.

However, `ToolRegistry::specs()` still exposes mutating tools to the model. The new system prompt additionally tells the model:

```text
use edit, write, and append...
prefer process...
use exec...
```

Then actual execution goes through:

```rust
execute_observed(...)
```

which uses the registry's default gate. With `auto_approve_mutating = false`, that is `DenyMutating`.

So the effective loop is:

```text
model is told "use edit"
      ↓
edit schema is advertised
      ↓
model correctly calls edit
      ↓
runtime refuses edit
```

For a robust frontier model this is wasteful. For a quantized/local model it is a recipe for repetition and hallucinated recovery.

There is an even more revealing inconsistency: `ToolRegistry::execute_with()` explicitly says it exists for interactive sessions where a human answers an approval question, but `rupi interactive` never uses it. I found no interactive approval flow wired into the turn loop.

So today:

```text
headless default     → mutating tools advertised, then denied
interactive default  → mutating tools advertised, then denied
```

rather than the much cleaner:

```text
headless default     → expose only actions that can actually execute
interactive default  → ask user for mutating action approval
trusted/auto mode    → expose and execute permitted mutating actions
```

I would fix the **tool capability contract as a whole**, rather than simply auto-approving more things.

The system prompt should also be generated from the actual model-visible tool set, similar in principle to Pi's `selectedTools` mechanism. It needs to react to policy filtering, interactive approval capability, progress-boundary narrowing, MCP activation, and failover to a model without tool support.

That should noticeably improve weak-model behavior even outside benchmarks.

---

### 3. P0: context thresholds do not follow a failover to a smaller model

This one directly affects the backup-model feature.

`open_session()` creates the context policy once:

```rust
ProfilePolicy::new(
  config.context_profile,
  provider.capabilities().context_window,
)
```

where `provider` is the primary.

`ProfilePolicy` stores absolute thresholds derived from that window.

After failover, `build_request()` correctly asks the **active provider** for its capabilities and constructs `ContextState` with the active context window. But `ProfilePolicy::evaluate()` still uses the absolute thresholds calculated from the original primary.

For example, with the balanced profile:

```text
primary: 262k context
    compact threshold ≈ 64k cap
    recent target     ≈ 16k

backup: 32k context
    correct compact threshold ≈ 24k
    correct recent target     ≈ 8k
```

After takeover, Rupi still uses roughly the first set.

`rebudget()` helps once at the transition, but the backup remains active afterwards. Subsequent tool cycles or later turns can regrow the working context under primary-sized thresholds and hit provider overflow before the proactive context policy thinks anything is wrong.

That is particularly undesirable for the exact use case you described earlier: a thinner or less reliable backup model stepping in when the primary fails.

The clean fix is to make policy evaluation **window-relative to the currently active model epoch**. `ContextState` already carries the necessary window. Static profile thresholds should be re-derived or safely clamped from that window on evaluation.

`AdaptiveContextPolicy` needs the same treatment. Ideally latency knees should also be scoped by model identity; a knee observed on a large primary should not automatically define the working characteristics of a completely different backup model.

---

### 4. P0/P1: Rupi is still much less forgiving than Pi of imperfect tool calls

This is the most important new finding specifically for fragile/quantized models.

Rupi's OpenAI-compatible decoder currently requires:

* a tool-call ID;
* strict valid JSON arguments;
* unambiguous indexing.

For malformed arguments:

```rust
serde_json::from_str(&builder.arguments)
```

fails the **entire model request** with `ModelFailureKind::Protocol`.

Protocol failures are deliberately not retried against the same model. With no backup, the turn ends.

Current Pi takes a considerably more tolerant approach. Its OpenAI-compatible path maintains partial tool-call state and uses `parseStreamingJson()`, which attempts normal JSON parsing, repairs common malformed string escaping/control characters, then tries partial-JSON parsing. Pi separately guards the dangerous case where output was truncated at the token limit so incomplete salvaged tool calls are not executed.

I would not copy permissive salvage blindly—especially for mutating calls. But Rupi should distinguish:

```text
transport/provider protocol is broken
```

from:

```text
the model generated a malformed tool invocation
```

The latter is normal enough for small local models that the harness should help recover.

A safer Rupi strategy would be:

```text
Malformed but identifiable model tool call
        ↓
do NOT execute it
        ↓
produce a model-visible failed/refused tool result
    "arguments were invalid JSON / schema-invalid..."
        ↓
allow the same model to correct the invocation
```

For benign JSON defects such as raw control characters or malformed backslash escaping, a conservative repair pass can happen before declaring it invalid.

I would also support tool-call correlation by **index or ID**, rather than defaulting every missing index to `0`, and synthesize a stable internal correlation ID when the provider omits one. The provider's ID does not need to be trusted as the sole source of harness identity.

This could be one of the highest-leverage improvements for heavily quantized models.

---

### 5. P1: context pressure is still calculated from only conversation messages

This is related to, but separate from, the failover issue.

`build_request()` currently tells the policy:

```rust
state.estimated_tokens = estimate_messages(&self.messages);
```

But the actual request includes:

```text
system prompt
+ messages
+ all exposed tool schemas
```

Rupi already has `estimate_tokens(&ModelRequest)`, which includes all three.

That means the policy can believe a request is comfortably below a threshold while the actual serialized request is significantly larger. The effect grows with MCP tools and hurts small-context local models most.

Because `assemble_request()` does not itself invoke context policy, the fix is straightforward conceptually:

```rust
let pre_policy_request = self.assemble_request(self.messages.clone());
state.estimated_tokens = estimate_tokens(&pre_policy_request);
```

Then apply the policy and assemble the final request again if it changed context.

This also gives one consistent definition of “working context” throughout the runtime.

---

### 6. P1: the headless budget fix records the wrong session-end semantics

The process-exit part of Round 1 is fixed: a durably recorded `BudgetExhausted` no longer exits as an ordinary error.

But `SessionHandle::turn()` now throws away the `TurnReport`:

```rust
self.turn_with(prompt, &CancelToken::new())?;
Ok(())
```

and the one-shot caller subsequently does:

```rust
session.close()
```

which records:

```rust
SessionEndReason::UserExit
```

Yet the event schema itself says:

```rust
UserExit
// The user asked to exit.

Interrupted
// A turn stopped with durable state intact and may be continued with --resume.
// ... a budget exhaustion ...
```

So the new implementation produces a contradictory canonical trace:

```text
turn_completed = budget_exhausted
session_ended  = user_exit
```

This is exactly the kind of semantic ambiguity Rupi generally works hard to avoid.

I would preserve exit status 0 if that is the intended scripting contract, but close a one-shot exhausted session as:

```rust
SessionEndReason::Interrupted {
  message: "model request budget exhausted"
}
```

and reserve `UserExit` for an actually completed one-shot or explicit interactive exit.

---

### 7. Smaller provider/accounting gap: cache dialects still trail Pi

The new cache accounting is now internally sound and the benchmark comparison is genuinely much better.

But Rupi currently recognizes essentially:

```text
prompt_tokens_details.cached_tokens
input_tokens_details.cached_tokens
cache_read_tokens
```

Current Pi additionally recognizes OpenAI-compatible variants including:

```text
prompt_cache_hit_tokens
cached_tokens   // top-level
```

These show up in DeepSeek-/Kimi-style APIs and compatible servers.

This is not urgent for the llama.cpp benchmark, but if the goal is “no weaker than Pi” across OpenAI-compatible local/cloud endpoints, I would add them while the usage-normalization work is still fresh.

---

## What I would put in the Round-2 implementation PR

I would keep the next patch focused on harness invariants rather than individual benchmark failures:

1. Make same-turn compaction unable to cross *any* `Unknown`/unresolved tool lifecycle; make context thresholds active-model/window aware; wire a real model-visible tool capability contract with interactive approval and no guaranteed-refused tools; make malformed model-generated tool calls recoverable without ever executing uncertain repaired mutations. Then fold in exact-request context estimation, correct `Interrupted` session closure, and the missing cache dialect aliases as smaller adjacent fixes.

The key deterministic tests should include an `Unknown → succeeded` compaction sequence, a 262k-primary → 32k-backup multi-turn growth test, default headless versus interactive mutation behavior, malformed JSON tool-call self-correction, missing-index/missing-ID tool calls, a small-context request dominated by tool schemas, and a budget-exhausted trace ending in `Interrupted`.

After those pass, I would rerun **01, 02, 04, and 06 first**, using the corrected logical/uncached/cache metrics. If model-visible context plateaus and the traces no longer show harness-induced denials/protocol aborts, then run all ten.

### Round-2 assessment

PR #120 successfully addressed the architectural problems from Round 1; I don't see a reason to back out any of its main ideas. The next gap is now more subtle: **Rupi has the right mechanisms, but several mechanisms disagree about what is safe or possible**.

The most immediate fix is the `Unknown`-crossing compaction bug. After that, I would address **dynamic tool affordances + approval** and **active-model-aware context policy** before spending another full benchmark run. Those three are genuine harness weaknesses relative to the reliability target, not benchmark tuning.
