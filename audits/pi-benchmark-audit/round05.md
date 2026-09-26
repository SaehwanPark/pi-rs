## Round 5 audit

I re-audited current `main` at `473dca45` after PR #123. The Round-4 work is real: all seven requested areas were implemented, including full argument validation, enforced progress boundaries, replay-safety-aware failover, endpoint dialect options, strict tool sampling, bounded truncation recovery, and context-override normalization.

Round 5 is mostly about **new interactions introduced by those fixes**, plus a few remaining robustness gaps that become important for fragile local models.

| Priority | Finding | Main impact |
|---|---|---|
| **P0** | Output-limit recovery retries after partial text was already streamed to the user | CLI/TUI can display an abandoned partial answer followed by a second answer |
| **P0/P1** | Prompt budgeting and output budgeting are still disconnected | Requests can fit Rupi's prompt policy but still exceed the model's total context budget |
| **P1** | Endpoint-level output limits are invisible to runtime recovery logic | Wire behavior and runtime reasoning disagree |
| **P1** | `preserve_reasoning` is effectively inert for normal Rupi-generated history | Newly documented compatibility feature usually has nothing to replay |
| **P1** | Streaming response/tool-call accumulation has no aggregate bound | Broken/local providers can consume unbounded memory within one request |
| **P1** | Generic remote endpoints default to `ReasoningExposure::Native` | Reasoning provenance is more confidently classified than configuration evidence justifies |
| **P1/P2** | An impossible progress boundary can burn the remaining request budget | Misconfiguration or unavailable mutation tools becomes a retry loop rather than an immediate diagnostic |
| **P2** | Token-estimator calibration and model-readable reduced-output recovery remain open | Still worthwhile, but no longer blockers |

### 1. P0: truncation recovery violates the user-visible streaming invariant

This is the most important Round-5 issue.

The new output-limit recovery is deliberately special:

```text
length/max_tokens
→ incomplete attempt omitted from future model context
→ compact prior history
→ retry once on same model
```

That is reasonable internally.

But `Collector` streams text immediately:

```rust
self.progress.on_text_delta(text);
```

and `CliProgress` forwards that directly to the live surface:

```rust
self.surface.text_delta(text)
```

Only **after the stream has finished** does Rupi learn that:

```rust
finish_reason == "length"
```

and enter `TurnFailure::OutputTruncated`.

So the observable sequence can be:

```text
stdout:
"I inspected the problem and the bug is cau"

provider: finish_reason=length

Rupi retries

stdout continues:
"The bug is caused by..."
```

The canonical model context may correctly exclude the first attempt, but the human-facing output is already corrupted.

This contradicts the previous safety invariant that replay after committed output is unsafe. PR #123 created a narrow exception at the model-context layer without also creating a transactional presentation layer.

#### Implementation approach

Separate three concepts that are currently conflated:

```rust
canonical_committed
model_context_committed
surface_committed
```

The simplest safe immediate rule is:

```text
output-limit recovery allowed only if no irreversible user-visible
assistant text has been emitted
```

That still permits useful recovery when the truncated response contained only hidden reasoning or incomplete tool calls.

A more capable long-term design is transactional response presentation:

```rust
progress.begin_model_attempt(id)

progress.provisional_text(delta)

on successful completion:
  progress.commit_model_attempt(id)

on truncation recovery:
  progress.abort_model_attempt(id)
```

A TUI can visually replace/remove a provisional attempt. Plain stdout cannot, so for a streaming headless surface you have two choices:

- buffer assistant text until the attempt is known-good; or
- disable truncation retry once text has been emitted.

I would choose the second initially. It retains low-latency streaming without lying about what stdout represents.

Add a regression using a real `CliProgress`/fake provider:

```text
attempt 1 -> "partial", length
attempt 2 -> "complete"
```

and assert stdout is **not** `"partialcomplete"`.

---

### 2. P0/P1: output reservation is missing from context budgeting

Rupi's context engine currently asks essentially:

```text
How large is the prompt?
```

but the provider request is actually:

```text
prompt + requested generation budget
```

For models with a shared context budget, those cannot be treated independently.

Current request sizing:

```rust
state.estimated_tokens = estimate_tokens(&estimated_request);
```

covers the prompt/system/tool schemas, but not reserved completion capacity.

And `assemble_request_for()` simply does:

```rust
request.max_output_tokens = capabilities.max_output_tokens;
```

without reducing that ceiling according to the prompt that is about to be sent.

Pi currently does the important extra step conceptually:

```text
available output
  = context window
  - estimated prompt
  - safety reserve
```

and clamps the requested maximum accordingly.

Consider a 32k model:

```text
prompt estimate     22k
configured output   16k
total potential     38k
context window      32k
```

Rupi can consider the prompt acceptable and send the request even though the requested generation budget makes the request impossible for providers that reserve `max_tokens` inside the context window.

#### Implementation approach

Introduce one explicit request-budget calculation before dispatch:

```rust
struct RequestBudget {
  prompt_tokens_est: u64,
  desired_output_tokens: Option<u64>,
  effective_output_tokens: Option<u64>,
  context_window: u64,
  safety_reserve: u64,
}
```

Then calculate approximately:

```text
remaining = context_window
          - prompt_estimate
          - safety_reserve

effective_output = min(desired_output, remaining)
```

Do not silently reduce below a minimum usable generation budget. If only 50 tokens remain, context reduction is usually better than asking a coding model to complete inside 50 tokens.

A reasonable decision sequence is:

```text
1. Assemble exact prompt.
2. Estimate prompt tokens.
3. Reserve safety margin.
4. Determine desired output.
5. If desired fits:
     keep it.
6. If a useful clamped output fits:
     clamp and record that fact.
7. Otherwise:
     compact before dispatch.
8. If still impossible:
     refuse.
```

Crucially, store both **desired** and **effective** output budgets. Round-4-style length recovery needs to know the difference.

This should also be applied during backup rebudgeting.

---

### 3. P1: `ModelEndpoint.max_output_tokens` and `ModelRequest.max_output_tokens` disagree

This is adjacent to Finding 2 but is a concrete bug independently.

`ProviderConfig::from_endpoint()` does:

```rust
max_output_tokens: endpoint
  .max_output_tokens
  .or(endpoint.capabilities.max_output_tokens)
```

So the provider can have an effective configured maximum.

But the runtime builds requests using only:

```rust
let max_output_tokens = capabilities.max_output_tokens;
...
request.max_output_tokens = max_output_tokens;
```

Therefore this configuration is possible:

```json
{
  "max_output_tokens": 8192,
  "capabilities": {
    "max_output_tokens": null
  }
}
```

Wire request:

```text
max_tokens = 8192
```

Runtime's `ModelRequest`:

```text
max_output_tokens = None
```

Then output-limit recovery performs:

```rust
let requested = request.max_output_tokens?;
```

and refuses recovery because it thinks no output ceiling was specified.

The provider and runtime are literally reasoning about different requests.

#### Implementation approach

There should be exactly one effective output limit before `ModelRequest` reaches the runtime loop.

Prefer normalizing it while constructing the provider/capability snapshot:

```text
endpoint.max_output_tokens
    overrides
capabilities.max_output_tokens
```

and expose that resolved value through the provider's capabilities or a dedicated request-budget API.

Even better, after Finding 2:

```rust
desired_output_tokens
effective_output_tokens
```

should be explicit fields produced by one shared resolver.

Do not let `request_body()` secretly add constraints that aren't represented in the `ModelRequest` the runtime audited.

General invariant:

> The wire mapper may translate a request, but it must not materially change its budgets.

---

### 4. P1: `preserve_reasoning` is wired in the adapter but not in normal session production

PR #123 added:

```json
"preserve_reasoning": true
```

and `mapping.rs` correctly knows how to replay:

```rust
ContentBlock::Reasoning(chunk)
```

when the chunk is native.

But normal successful model responses don't put reasoning into their assistant `Message`.

`Collector` records reasoning as trace events:

```rust
AgentEvent::ReasoningDelta(...)
```

while `Response` carries only approximately:

```text
text
calls
rejected_calls
reasoning_provenance
```

and `record_response()` constructs the assistant message from text and tool calls only.

So:

```text
provider emits native reasoning
       ↓
trace contains reasoning_delta
       ↓
assistant Message contains no Reasoning block
       ↓
next request has nothing for preserve_reasoning to replay
```

The mapper implementation works, but ordinary Rupi history doesn't supply it with data.

#### Implementation approach

If reasoning replay is desired, promote exposed reasoning into the completed assistant semantic message **only after successful completion**.

For example, Collector can accumulate structured chunks:

```rust
Vec<ReasoningChunk>
```

or one normalized block per provenance.

Then `Response` becomes roughly:

```rust
struct Response {
  text: Option<String>,
  reasoning: Vec<ReasoningChunk>,
  calls: Vec<ToolCallBlock>,
  ...
}
```

and `record_response()` stores:

```text
Reasoning(Native)
Text(...)
ToolCall(...)
```

in the original provider ordering if that ordering is semantically relevant.

Important constraints:

- only provider-exposed reasoning enters this path;
- preserve provenance;
- never turn `ProviderSummary` or `Declared` into `Native`;
- truncated/failed attempts remain trace-only;
- `preserve_reasoning=false` should still permit storage if canonical fidelity requires it, while the mapper omits it from future prompts.

That last distinction is cleaner: **storage policy and replay policy should not be the same thing**.

---

### 5. P1: aggregate streamed response size is still unbounded

The SSE framing itself is nicely bounded:

```rust
MAX_EVENT_BYTES = 1 MiB
```

But that only limits one SSE event.

The decoder can accumulate arbitrarily many events into:

```rust
ToolBuilder.arguments: String
```

and the runtime Collector similarly accumulates:

```rust
text: String
```

There is no obvious aggregate maximum for:

- assistant text per request;
- reasoning text per request;
- arguments per tool call;
- number of tool calls in one response;
- total tool-call argument bytes.

A pathological local server could emit:

```text
10,000 × 100 KiB argument fragments
```

while every individual SSE event remains under the 1 MiB bound.

Even without hostility, broken quantized/template behavior can generate bizarre repeated tool payloads.

#### Implementation approach

Add adapter-level response limits, not just SSE framing limits:

```rust
struct ResponseLimits {
  max_text_bytes: usize,
  max_reasoning_bytes: usize,
  max_tool_calls: usize,
  max_tool_argument_bytes_per_call: usize,
  max_tool_argument_bytes_total: usize,
}
```

Defaults should be generous enough for legitimate code generation but finite.

I would enforce tool-call limits in the decoder before strings grow:

```rust
if builder.arguments.len() + fragment.len() > limit {
  mark call rejected / terminate response safely
}
```

and enforce tool-count limits before allocating new builders.

For ordinary visible text, exceeding the cap is trickier because some content may already have been streamed. Once user-visible output exists, classify it as an incomplete semantic response and **do not retry automatically**.

For tool-call overflow, no partially accumulated call should ever execute.

This should be tested with thousands of tiny SSE chunks, not merely one oversized event.

---

### 6. P1: generic endpoint constructors overclaim native reasoning provenance

Both:

```rust
ModelEndpoint::local(...)
```

and:

```rust
ModelEndpoint::remote(...)
```

currently default to:

```rust
exposed_reasoning: ReasoningExposure::Native
```

That is too strong for a generic “OpenAI-compatible” constructor.

`ReasoningExposure` itself correctly says:

```rust
None
```

means no provenance claim has been established.

Yet the generic remote constructor immediately upgrades the endpoint to `Native`.

For a local llama.cpp model whose template truly exposes model reasoning, this may be correct. For an arbitrary OpenAI-compatible gateway it may not be. Some endpoints can expose summarized/processed reasoning-like content instead.

That matters because PR #123 now also offers reasoning preservation/replay.

#### Implementation approach

Make generic constructors conservative:

```text
local generic  → None
remote generic → None
```

Then let configuration explicitly declare:

```json
"exposed_reasoning": "native"
```

for verified endpoints.

Alternatively provide named convenience constructors/profiles:

```rust
ModelEndpoint::llama_cpp(...)
ModelEndpoint::openai_compatible(...)
```

where only the verified profile supplies stronger defaults.

Also validate:

```text
preserve_reasoning = true
```

against:

```text
exposed_reasoning == Native
```

If they disagree, either reject config or disable replay with a diagnostic. I prefer rejecting contradictory configuration.

This follows the architecture's broader rule: provenance claims should arise from configuration/evidence, not field-name optimism.

---

### 7. P1/P2: an unsatisfiable progress boundary can waste all remaining requests

PR #123 correctly rejects text-only completion while a progress boundary is active.

But suppose the boundary activates and the effective progress tool set is empty because:

- configured `progress_tool_names` don't exist;
- they're denied by policy;
- they're mutating but the current surface cannot approve them;
- dynamic tool availability changed;
- failover capability/filtering removed them.

`assemble_request_for()` then produces:

```rust
tool_choice = ToolChoice::Auto
```

because `tools.is_empty()`.

The model answers text.

Rupi rejects the completion.

Then repeats until request budget exhaustion.

The final status is honest, but several useless model requests were consumed for a condition the harness already knew was impossible.

#### Implementation approach

When activating a progress boundary, resolve its **effective executable tool set** immediately.

If empty:

```text
do not contact the model again
```

Emit a durable diagnostic such as:

```text
progress boundary cannot be satisfied:
configured progress tools are unavailable under the current
model/tool/approval policy
```

and finish incomplete.

Potentially distinguish:

```rust
ProgressBoundaryState {
  Inactive,
  Active { tools: Vec<String> },
  Unsatisfiable { reason: String },
  Satisfied,
}
```

rather than representing all of this with two booleans.

For interactive mode, tools requiring approval are satisfiable. For headless mode without auto-approval, they aren't.

Tests should include denied, unknown, read-only, no-tool backup, and approval-unavailable cases.

---

## Two previously deferred items now worth scheduling

The **model-scoped token calibration** from Round 4 remains useful. Now that the stale-measurement bug is gone, you can safely learn a bounded correction ratio:

```text
provider measured prompt
------------------------
local bytes/4 estimate
```

per model and apply it to future *current-request* estimates. Use a conservative EMA/clamped percentile rather than directly replacing estimates.

And the full-output archive remains model-inaccessible. Rupi already has the harder part—durable blobs and recovery references. A small read-only tool such as:

```text
payload_read(ref, offset?, limit?)
```

would let the model inspect an elided command result without rerunning an expensive or mutating command.

## Recommended Round-5 implementation slice

I would implement in this order:

1. **Fix truncation recovery versus live streaming first.** Do not retry after irreversible surface text emission until presentation attempts become transactional.
2. **Unify prompt + output budgeting.** Introduce desired/effective output limits and clamp against remaining context.
3. **Eliminate the endpoint/request max-output split.** The runtime must know the exact wire ceiling.
4. **Make reasoning preservation real.** Persist exposed native reasoning into completed semantic assistant messages, while keeping replay opt-in.
5. **Bound aggregate provider output/tool-call accumulation.**
6. **Make reasoning provenance defaults conservative.**
7. **Fail fast on unsatisfiable progress boundaries.**

After those fixes I would run the deterministic fault-injection suite with several especially nasty cases:

```text
partial visible answer → length stop
32k context + 24k prompt + 16k desired output
endpoint-only max_output_tokens
2,000 fragmented tool calls
single tool call with multi-megabyte fragmented args
preserve_reasoning across two model rounds + resume
progress boundary with zero executable mutation tools
```

At that point I would be comfortable moving the audit emphasis away from core safety invariants and much more heavily toward **empirical weak-model performance**—Qwen/DeepSeek/GLM-style quantized models, malformed tool rates, requests-to-completion, context efficiency, and whether constrained sampling actually improves task completion.

Round 5 therefore looks different from the early rounds: I no longer see a broad architectural deficiency comparable to Rounds 1–3. The remaining high-priority issues are mostly **cross-layer contract leaks introduced by increasingly sophisticated recovery/compatibility features**. That is a considerably better place for the project to be.
