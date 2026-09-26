## Round 3 audit

I audited current `main` at `efd8740` after PR #121. The Round-2 work is substantive and, on source inspection, the seven intended fixes are present. The earlier `Unknown`-crossing compaction bug is fixed, approval is genuinely wired into interactive execution, active-model context thresholds are implemented, malformed tool calls now become corrective tool results rather than immediate turn failures, full-request sizing exists, interrupted session semantics are corrected, and the cache aliases landed.

At this point I would move past the Round-2 issues. Round 3 exposed two new **P0 runtime invariants** and two important **P1 local-model/MCP robustness gaps**.

| Priority | Finding                                                                                                | Consequence                                                                                     |
| -------- | ------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------- |
| **P0**   | Context policy uses the previous request's token measurement as if it described the current request    | Context control remains one request stale; PR #121's full-request estimator is often bypassed   |
| **P0**   | Cancellation/finalization can leave an assistant tool batch without complete model-visible results     | Subsequent requests/resume can contain invalid tool protocol history                            |
| **P1**   | Provider tool-fragment correlation is now safer but overly strict for one unambiguous outstanding call | Fragile OpenAI-compatible servers generate avoidable failed calls                               |
| **P1**   | Dynamically discovered MCP tools have no strong name/schema admission boundary                         | Invalid names, system-prompt contamination, or schema explosions can break local models/context |

### 1. P0 — context policy is still evaluating the previous request

This is the biggest Round-3 finding.

PR #121 correctly changed `build_request()` to calculate the full current request:

```rust
let estimated_request = self.assemble_request(self.messages.clone());
let estimated_tokens = estimate_tokens(&estimated_request);
```

That now includes the system prompt, messages, and exposed tool schemas.

But immediately afterward:

```rust
state.measured_tokens = self.measured_input_tokens;
state.estimated_tokens = estimated_tokens;
```

and `ContextState` says:

```rust
pub fn effective_tokens(&self) -> u64 {
  self.measured_tokens.unwrap_or(self.estimated_tokens)
}
```

The problem is what `self.measured_input_tokens` actually means. It is assigned **after the previous successful model request**:

```rust
self.measured_input_tokens =
  usage.logical_prompt_tokens.or(usage.input_tokens);
```

Therefore request $N+1$ effectively asks the policy:

```text
current request estimate:  47k
previous request measured: 19k

effective_tokens() = 19k
```

The new 47k estimate is ignored.

This matters immediately after a large tool result, dynamic MCP activation, a change in exposed tools, an injected checkpoint/summary, or any other material working-set change. It also works badly in the opposite direction: after compaction shrinks the context, an old large measurement can keep reporting artificial pressure.

Failover makes it more problematic. `measured_input_tokens` is not reset on manual switch, switch-back, or automatic failover. A token count measured by model A can therefore be treated as the authoritative size of the next request to model B, even though they may have different tokenizers and different exposed capabilities.

I would change the abstraction rather than merely clear this field. A provider measurement is evidence about the **request that was just sent**, not a measurement of the request currently being constructed.

A useful design would retain something like:

```text
last_model
last_estimated_tokens
last_measured_tokens
```

and derive a model-specific estimator calibration from those values. For example, the current request can use:

```text
current_estimate × previous_measurement / previous_estimate
```

within conservative bounds, but only when the model identity is unchanged. A model switch starts fresh.

For the first fix, even simply making the **current assembled estimate authoritative** is safer than the present stale-measurement behavior.

There is an adjacent failover issue in `rebudget()`: it still decides whether the backup fits using:

```rust
estimate_messages(&self.messages)
```

rather than the backup's complete request shape. This again omits system + tool schemas. Once switching to a smaller model, Rupi should validate:

```text
backup system
+ retained messages
+ tools actually exposed to backup
```

against the backup window before committing the epoch transition.

I would cover all of this under one invariant:

> Context policy must operate on the request that is about to be sent, using measurements only to calibrate that request's estimate and never as a stale substitute for it.

---

### 2. P0 — a cancelled tool batch can corrupt the model-visible protocol

This is more serious than it initially looks.

Rupi first persists the entire assistant response:

```text
assistant
  tool_call A
  tool_call B
  tool_call C
```

Then `execute_calls()` processes them sequentially.

If cancellation is noticed at B:

```rust
if cancel.is_cancelled() {
  ...
  self.push_message(tool_result_for_b, ...);
  break;
}
```

C is never processed.

The resulting model-visible history becomes approximately:

```text
assistant -> A, B, C
tool -> result A
tool -> cancellation result B
```

There is **no result for C**.

The next model request can therefore contain an assistant tool-call batch that is incomplete according to the OpenAI-style tool protocol. More importantly, Rupi's resume reconciliation cannot necessarily repair this: C never received a `ToolRequested` lifecycle event, so it is not an interrupted execution that the existing lifecycle scanner knows about.

There is a second bug in the same cancellation path. The synthetic result for B is:

```rust
state: ToolExecutionState::Requested
```

despite also emitting:

```rust
AgentEvent::ToolFailed(...)
```

`Requested` is explicitly non-terminal in Rupi:

```rust
pub fn is_terminal(self) -> bool {
  matches!(self, Self::Succeeded | Self::Failed | Self::Unknown)
}
```

So an operation that Rupi knows with certainty **never started** becomes model-visible as a non-terminal lifecycle. That can permanently prevent later same-turn compaction from crossing it.

There is a third manifestation in `record_unexecuted_calls()`, used when budget finalization asks for tools despite tools being disabled. It emits `ToolRequested` and `ToolFailed` canonical events but **no model-visible `Role::Tool` result message at all**. Yet the assistant message containing those calls remains in history. Resume can therefore encounter the same invalid protocol shape.

I would establish a hard runtime invariant:

> Once an assistant message containing tool calls is committed to model-visible history, every call in that assistant message must eventually receive exactly one corresponding model-visible tool result before another provider request is allowed.

Cancellation should settle the entire unexecuted tail. For A/B/C, if A completed and cancellation then occurs:

```text
A -> actual result
B -> terminal Failed: not executed because turn was cancelled
C -> terminal Failed: not executed because turn was cancelled
```

`Failed` is appropriate for a call proven not to have begun. Reserve `Unknown` for execution that may actually have crossed the side-effect boundary.

The same helper should handle finalization-disabled calls, cancellation-before-first-call, approval aborts if they ever terminate a batch, and similar early exits.

This is especially relevant to Rupi's product goal because graceful interruption/abort was one of the major reasons for designing the durable lifecycle machinery in the first place.

---

### 3. P1 — malformed-call recovery became unnecessarily strict about streaming correlation

The general PR #121 design is good: malformed JSON or genuinely ambiguous call fragments should no longer crash the turn, and rejected calls must never execute.

I would keep that.

But the implementation rejects this pattern:

```text
chunk 1:
  index=0
  id=abc
  name=read
  args={"path":

chunk 2:
  no index
  no id
  args="file.rs"}
```

even when **exactly one tool call is open**.

The decoder deliberately marks a fragment having neither index nor ID as a correlation error. That is safe, but unnecessarily strict for local OpenAI-compatible servers.

There is no ambiguity when:

```text
number of currently open, non-conflicted builders == 1
```

In that specific case, Rupi can safely associate the fragment with that sole builder.

The rule I would use is:

```text
explicit index/id match             -> attach
one and only one outstanding call   -> attach as compatibility fallback
zero candidates                     -> reject
multiple candidates                 -> reject
conflicting index/id                -> reject
```

Do **not** broaden it beyond the unique-singleton case.

This small tolerance improvement is exactly the sort of harness behavior that can make moderately quantized models and imperfect llama.cpp/vLLM-compatible servers noticeably less brittle without sacrificing execution safety.

---

### 4. P1 — MCP metadata needs an admission boundary before becoming model-visible

PR #121's dynamic tool guidance introduced a new boundary:

```rust
"Tools available for this request: {names}. ..."
```

That is useful, but `names` is assembled directly from tool metadata.

For MCP tools:

```rust
format!("mcp__{server_name}__{tool_name}")
```

uses the configured server name and server-provided MCP tool name essentially verbatim.

Meanwhile `RuntimeConfig::validate()` only checks that an MCP server name is non-empty and unique. `McpManager::enable_server()` accepts every returned tool, description, and `inputSchema`, and `register_shared()` inserts it directly into the registry.

That creates two problems.

First, external text can now reach a **system-role instruction** through the tool name. A broken or hostile MCP endpoint returning a name containing line breaks or instruction-like text should never be able to transform:

```text
Tools available for this request: ...
```

into new arbitrary system-prompt lines.

Second, there is no apparent admission budget for:

```text
number of discovered tools
description size
schema size/depth
aggregate exposed schema footprint
```

A server exposing 300 enormous schemas can make an otherwise empty request too large. The context engine cannot solve this by compacting conversation history because the pressure comes from the tool definitions themselves.

I would therefore put a normalization/admission layer between MCP discovery and `ToolRegistry`:

```text
MCP response
    ↓
validate canonical name
validate schema shape
bound description/schema size
bound server/aggregate tool count
derive provider-safe namespaced name
    ↓
ToolRegistry
```

For over-budget MCP capability sets, refuse or selectively expose them with a diagnostic. Don't silently stuff them all into every request and hope context compaction compensates.

This is also a good place to implement the capability-exposure ideas already present in `rupi-experiments`: large MCP surfaces should eventually be **selectively activated**, which is especially valuable for smaller local models.

---

## What I would implement in Round 3

I would make the next PR primarily about two runtime invariants, with the provider/MCP changes alongside them:

1. **Current-request context truth:** replace stale `measured_tokens` semantics with model-scoped estimator calibration; size failover against the actual backup request.
2. **Complete tool batches:** every persisted assistant tool call receives exactly one terminal model-visible result, including cancellation and no-tool finalization.
3. Relax stream correlation only for the provably unique single outstanding call.
4. Add MCP tool-name normalization plus bounded metadata/schema admission.

The most valuable deterministic regression test is probably a single scenario combining the first two:

```text
assistant emits 3 tool calls
tool 1 succeeds
cancellation arrives
tools 2 and 3 never execute
turn ends Cancelled
session receives another user turn
```

Assert that both calls 2 and 3 have terminal failed results, no unmatched assistant tool calls exist, the next provider request is protocol-valid, and later compaction can cross those known-nonexecuted calls.

For context, test:

```text
request 1 measured = 8k
tool result/schema change makes request 2 estimate = 30k
```

and prove the policy sees ~30k, not 8k. Then repeat across a primary → smaller-backup switch.

### Where this leaves Rupi

The trend across the three rounds is good: the findings are moving from missing core capabilities to **cross-component invariants and edge conditions**. I no longer see the original architectural disadvantage where Rupi simply could not manage a long active coding turn.

I still would **not declare parity with Pi yet**, mainly because PR #121 could not rerun even the targeted 01/02/04/06 model experiments. After the two P0 fixes above, I think another targeted benchmark run becomes worthwhile rather than continuing source-only hardening indefinitely. The next empirical question should be whether Rupi's corrected context machinery actually reduces request growth and whether the same local model now completes more work per request, rather than chasing individual test-case failures.
