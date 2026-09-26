## Round 4 audit

I re-audited current `main` after PR #122. The Round-3 fixes are present and directionally correct: current-request sizing now uses the assembled request, failover rebudget includes the backup request shape, cancelled/unexecuted tool batches are settled, single unkeyed fragments get the intended compatibility fallback, and MCP admission is bounded.

Round 4 is now exposing problems at the **model ↔ harness contract boundary**, which is exactly where fragile and quantized local models tend to suffer. I found two issues I would treat as immediate, followed by several P1 gaps that are worth fixing before claiming parity with Pi.

| Priority | Finding | Main risk |
|---|---|---|
| **P0** | Optional tool arguments are not actually schema-validated | Malformed model calls can execute with different semantics than the model requested |
| **P0/P1** | Progress boundary can be bypassed by a plain-text answer | A weak model can claim completion without making the progress Rupi explicitly required |
| **P1** | Same-model retry conflicts with OpenAI adapter quarantine | “Retry” can consume budget without contacting the model |
| **P1** | OpenAI-compatible dialect configuration is mostly unreachable from `RuntimeConfig` | Real local servers cannot use adapter compatibility knobs that already exist |
| **P1** | No provider-assisted strict/constrained tool sampling | Rupi leaves avoidable JSON/tool-call errors to weak models |
| **P1** | Output-limit responses have no bounded recovery path | Reasoning-heavy local models can lose a turn at `finish_reason=length` |
| **P1** | Context overrides disappear under adaptive mode or different-window failover | Explicit operator configuration does not mean what it says |
| **P2** | Token estimation remains raw bytes/4; reduced output is not directly rehydratable by the model | Small-context/CJK robustness and debugging efficiency remain weaker than they need to be |

### Finding 1 — P0: malformed optional tool arguments can silently change execution semantics

This is the most concerning issue in Round 4.

`ToolRegistry::validate_arguments()` currently validates that the top-level arguments are an object, then iterates only over properties named in `"required"`:

```rust
let Some(required) = schema.get("required").and_then(|v| v.as_array()) else {
  return Ok(());
};

for key in required {
  ...
  // type validation
}
```

So optional arguments are effectively not schema-validated.

That becomes dangerous because several built-ins intentionally interpret missing optional values as defaults. Consider `exec`:

```json
{
  "command": "some command",
  "cwd": 123
}
```

Its schema declares:

```json
"cwd": { "type": "string" }
```

but `cwd` is optional. Therefore registry validation accepts the call.

Then `ExecTool::preflight()` does:

```rust
request.arguments
  .get("cwd")
  .and_then(|value| value.as_str())
```

The integer becomes `None`, which is interpreted as though `cwd` was never supplied. Execution later makes exactly the same conversion and therefore runs the command in the **workspace root**.

For a heavily quantized model, emitting the right field with the wrong JSON type is completely plausible. The correct harness behavior is:

```text
model emits cwd=123
        ↓
failed tool result:
"cwd must be a string"
        ↓
model corrects call
```

not:

```text
cwd=123
        ↓
silently treat as missing
        ↓
run mutating command somewhere else
```

The same pattern exists around optional integers/booleans in `read`, `grep`, `exec`, `process`, and `edit`.

**Implementation approach.** Make the registry's pre-execution schema boundary authoritative over every *supplied* property. I would cache metadata/schema validation state at registration rather than repeatedly fetching it:

```rust
struct RegisteredTool {
  tool: Arc<dyn Tool>,
  metadata: ToolMetadata,
  schema: Value,
  validator: ToolArgValidator,
}
```

For the built-ins, a lightweight validator only needs a useful JSON-Schema subset initially: `type`, `required`, `properties`, nested `items`, `enum`, and `additionalProperties`. Add `"additionalProperties": false` to Rupi-owned built-in schemas. An optional property then means “may be absent,” never “may have the wrong type.”

For MCP schemas, you have two reasonable routes. A full JSON-Schema validator compiled lazily when an MCP server activates is cleanest; alternatively, retain the lightweight validator but explicitly document which schema constraints it enforces. I would not compile heavyweight validators on every tool invocation.

The important ordering already exists and should be preserved:

```text
schema validation
→ tool-specific preflight
→ human/automatic approval
→ ToolStarted
→ execution
```

Regression tests should include malformed `exec.cwd`, `exec.timeout_ms`, `process.cwd`, `process.args[*]`, `read.offset`, `grep.ignore_case`, unknown arguments on built-ins, and nested array item types. Most importantly, assert that no external effect or `ToolStarted` occurs.

---

### Finding 2 — P0/P1: the progress boundary does not actually require progress

The comments and model-visible instruction describe a strong contract. `with_progress_boundary()` says it will:

> Require a configured kind of tool progress...

and when activated, Rupi tells the model:

> “the turn remains incomplete until the change is attempted.”

Rupi then narrows the next request's tool list appropriately.

But after receiving the response, the main loop does essentially:

```rust
let RecordedResponse { calls, .. } =
  self.record_response(response, &mut report)?;

if calls.is_empty() {
  return self.finish(
    report,
    TurnStatus::Completed,
    ...
  );
}
```

There is no check for:

```rust
self.progress_boundary_active
```

before accepting a text-only answer as completion.

So this sequence succeeds:

```text
model spends inspection budget
→ progress boundary activates
→ only `edit`/`write` is exposed
→ model ignores tools and says "Done, I've fixed it."
→ Rupi: TurnStatus::Completed
```

That is precisely the behavior the feature is supposed to guard against, especially with unreliable local models.

There is even a nearby test where progress calls fail, the boundary remains narrowed, the model subsequently says `"done"`, and the turn is still asserted `Completed`.

**Implementation approach.** Enforce the boundary as a runtime postcondition, not merely a prompting/tool-exposure hint.

Introduce explicit request tool-choice semantics in core:

```rust
pub enum ToolChoice {
  Auto,
  None,
  Required,
  Specific(String),
}
```

When the progress boundary is active, send `Required` where the provider supports it. Current llama.cpp accepts `tool_choice: "required"` as a defined choice. :chatgpt-content-reference{index="0"}

But do **not** trust that alone. There are fresh reports from September 2026 that some llama.cpp/Qwen3/template combinations can still produce prose without a tool call under `"required"`. :chatgpt-content-reference{index="1"} Rupi therefore still needs its own semantic enforcement.

When:

```text
progress_boundary_active == true
&& calls.is_empty()
```

do not return `Completed`. Record the assistant response as a non-terminal response, append a runtime-owned corrective message, and consume another request if budget remains. If the ordinary request budget runs out without successful progress, finish as `BudgetExhausted`, not `Completed`.

Also avoid adding that premature `"done"` text to `TurnReport.text` as though it were the final answer. The canonical trace can retain it; the final report should distinguish intermediate rejected completion claims from the accepted terminal response.

This deserves tests for text-only bypass, unknown-tool bypass, repeatedly failed progress tools, and budget exhaustion while the boundary remains unsatisfied.

---

### Finding 3 — P1: retry policy and provider quarantine disagree

The architecture is conservative about ambiguous HTTP failures, which I agree with.

`OpenAiCompat` quarantines itself after transport/timeout/cancellation cases where a POST may already have reached the server:

```rust
self.quarantined.store(true, Ordering::Release);
```

and explicitly documents:

> automatic retries inside the previous turn never reach this hook.

The quarantine is cleared only at the beginning of a **new user turn** by:

```rust
provider.reset_after_abandonment();
```

But `FailoverPolicy` still classifies `Transport` and `Timeout` as retryable, so the current turn does:

```text
request → ambiguous transport failure
adapter becomes quarantined
runtime emits ModelRetry
waits for backoff
runtime invokes same adapter
adapter immediately returns "quarantined"
```

That second “request” consumes the model-request budget without performing another network inference.

This is safe with respect to duplicate side effects, but it makes tracing, retry counts, latency, and availability behavior misleading.

**Implementation approach.** Failure kind is not enough to decide replayability. Add request-level replay certainty:

```rust
pub enum RequestReplaySafety {
  Safe,
  AmbiguousPostBoundary,
  CommittedOutput,
}
```

or equivalent metadata on `ModelFailure`.

Then define:

```text
connection failure before request dispatch  → Safe
HTTP 429 / explicit 5xx response            → Safe
POST sent, socket disappears                → AmbiguousPostBoundary
idle/total timeout after dispatch            → AmbiguousPostBoundary
assistant/tool output already committed      → CommittedOutput
```

`FailoverPolicy` should retry the same model only when both:

```rust
kind.is_retryable()
&& failure.replay_safety == Safe
```

For `AmbiguousPostBoundary`, skip the fake local retry and move directly to the configured failover decision or stop.

I would retain the existing quarantine behavior; it is the policy that should become aware of why the adapter is quarantined.

A particularly useful deterministic test is to count actual mock HTTP requests. After an ambiguous post-boundary timeout, the trace must contain no `ModelRetry` claiming a same-model retry that did not hit the server.

---

### Finding 4 — P1: the OpenAI-compatible adapter has compatibility knobs users cannot configure

`ProviderConfig` already contains useful interoperability settings such as:

```text
stream
max_tokens_field
thinking_input
headers
```

and even comments that non-streaming mode is a workaround for servers with broken SSE.

But `RuntimeConfig` configures endpoints through `ModelEndpoint`, which exposes none of those fields. `ProviderConfig::from_endpoint()` copies the fields that exist and then gets the others from:

```rust
..Self::default()
```

So much of the apparent configurability in `rupi-provider` is not reachable from the normal Rupi CLI/config path.

This matters because “OpenAI-compatible” local endpoints are not actually one dialect.

There is also a concrete thinking-control problem. Under `ThinkingInput::ReasoningEffort`, Rupi currently handles `ThinkingLevel::Off` by sending **no field**. Current llama.cpp explicitly recognizes:

```json
"reasoning_effort": "none"
```

as the instruction to disable thinking. :chatgpt-content-reference{index="2"} Omitting the parameter may leave the template/server default in effect instead.

Current llama.cpp also exposes reasoning budgets and reasoning preservation as explicit capabilities. :chatgpt-content-reference{index="3"}

**Implementation approach.** Add a nested compatibility object to `ModelEndpoint`; do not continually add provider quirks as unrelated top-level fields. For example:

```rust
pub struct OpenAiCompatOptions {
  pub stream: Option<bool>,
  pub stream_usage: Option<bool>,
  pub max_tokens_field: Option<MaxTokensField>,
  pub thinking_input: Option<ThinkingInput>,
  pub thinking_disable: Option<ThinkingDisableMode>,
  pub thinking_budget_field: Option<ThinkingBudgetField>,
  pub requires_tool_result_name: Option<bool>,
  pub preserve_reasoning: Option<bool>,
  pub strict_tools: Option<StrictToolSupport>,
}
```

Move the relevant public enums into `rupi-core` if necessary to preserve dependency direction.

Then make `ProviderConfig::from_endpoint()` explicitly resolve every option. Add wire-level tests proving that config JSON actually changes the outgoing request.

For llama.cpp specifically, `ThinkingLevel::Off` under the reasoning-effort dialect should resolve to `"none"` rather than omission. Don't globally assume that for every OpenAI-compatible endpoint—make the disable encoding part of the dialect contract.

I would also add support for conditional reasoning replay. Rupi currently discards all reasoning blocks from prior assistant messages. Current llama.cpp supports templates where preserving reasoning across history is meaningful, and its own current tool-loop tests preserve `reasoning_content` in assistant history when present. :chatgpt-content-reference{index="4"} That does **not** mean Rupi should replay reasoning everywhere; it argues for a provider/model compatibility switch instead of one universal rule.

---

### Finding 5 — P1: Rupi does not expose provider-assisted constrained tool sampling

Current Rupi tool definitions contain:

```text
name
description
parameters
```

but no statement that a tool wants schema-constrained generation.

Current Pi's core coding tools such as `read`, `write`, `edit`, and shell execution mark their tool input as JSON-schema constrained with a “prefer” policy. Provider adapters enable strict behavior only when the provider/model is known to support it.

That distinction is useful for your target users. Runtime validation remains the final safety boundary, but grammar/schema-constrained generation can prevent many malformed calls from being generated in the first place.

**Implementation approach.** Extend `ToolSpec` with an optional model-generation constraint, for example:

```rust
pub enum ToolSamplingConstraint {
  None,
  JsonSchema {
    strict: Strictness,
  },
}

pub enum Strictness {
  Prefer,
  Require,
}
```

Rupi-owned built-ins can request `Prefer`.

Then add an endpoint capability such as:

```rust
strict_tool_schema: Unsupported | Supported
```

with these semantics:

```text
Prefer + supported     → emit provider strict/schema constraint
Prefer + unsupported   → ordinary tool schema fallback
Require + supported    → emit constraint
Require + unsupported  → refuse before model request
```

Do not simply add `"strict": true` universally. Generic OpenAI-compatible servers can reject fields they do not understand, and strict OpenAI-style schemas may require normalization of optional properties. Provider-specific conversion should own that transformation.

This should be implemented **after or together with Finding 1**. Provider-constrained generation is assistance; full pre-execution validation remains the authority.

---

### Finding 6 — P1: output-limit responses have no bounded recovery path

Rupi correctly refuses to execute tool calls from a response stopped at the output limit:

```rust
if usage.stopped_at_output_limit() {
  ModelFailureKind::Semantic
}
```

That safety rule should stay.

But the runtime then treats the semantic failure as terminal. There is no specific truncation recovery.

This is particularly relevant for thinking models. A model can spend most of its output budget reasoning and reach `finish_reason = length` before completing its answer or tool call.

Current Pi has a narrowly bounded recovery path: only certain `length` stops are classified as recoverable, the unusable attempt is omitted from projected context, compaction occurs, and one retry is allowed. Its `isRecoverableLength()` deliberately checks that output stopped below the originally desired output limit, avoiding an endless retry when the model simply consumed the true configured maximum.

Rupi is actually well positioned to implement this safely because a failed response is not currently inserted into normal model-visible history; its canonical deltas can remain in the trace.

**Implementation approach.** Add a distinct internal outcome such as:

```rust
TurnFailure::RecoverableOutputTruncation(...)
```

rather than routing every `length` stop through generic `Semantic`.

Use a one-shot `truncation_recovery_used` guard, analogous to your overflow-recovery guard.

The conservative initial eligibility should be something like:

```text
finish reason says output limit
same model
no tool from truncated response executed
no recovery already attempted
actual output < originally requested output ceiling
```

Then compact pre-existing context if useful and retry once. If the model genuinely used the full requested output ceiling, blindly repeating the same request is unlikely to help; finish honestly as incomplete instead.

For local reasoning models, a later enhancement can combine this with a provider-supported thinking budget so reasoning cannot consume the entire completion allowance.

Tests should distinguish “provider/context clamped below desired output” from “model consumed the real maximum.”

---

### Finding 7 — P1: context overrides are not stable configuration

`ContextOverrides` says:

> “Numeric overrides for experts who need them. Absent means profile-derived.”

But the current wiring gives them two surprising behaviors.

With ordinary `ProfilePolicy`, the overrides mutate the threshold values derived for the **primary** window. When the active model's window changes:

```rust
if state.window == self.reference_window {
  self.thresholds
} else {
  ContextThresholds::for_profile(self.profile, state.window)
}
```

the explicitly configured overrides disappear.

More directly, when:

```json
"adaptive_context": true
```

`run.rs` constructs `AdaptiveContextPolicy` and does not apply `context_overrides` at all.

So the same config option means different things depending on an unrelated mode switch.

**Implementation approach.** Make overrides first-class policy input rather than mutating one initial threshold object.

The evaluation pipeline should be:

```text
active model/window
        ↓
derive profile thresholds
        ↓
apply explicit ContextOverrides
        ↓
validate/clamp for active window
        ↓
if adaptive enabled:
  adaptive knee may cap/lower them further
        ↓
evaluate
```

Define and test the ordering invariant:

```text
warn <= reduce <= compact < checkpoint < active window
recent_target < compact
```

For a smaller backup window, I would not silently discard impossible overrides. Either clamp them with a durable diagnostic, or reject the failover if the configured policy explicitly cannot operate inside that window. Clamping is probably the friendlier default.

Given that adaptive mode is explicitly opt-in, it is reasonable to define adaptive thresholds as caps that may lower expert overrides; document that precedence.

---

## Two lower-priority robustness improvements

These do not block the next PR, but I would keep them on the roadmap.

First, PR #122 correctly removed the stale prior-request token count, but context sizing is now entirely a fixed:

```rust
bytes / 4
```

estimate. That remains weak for CJK, code, JSON schemas, and model-specific tokenizers. The Round-3 recommendation to retain a **model-scoped calibration ratio** is still worthwhile. Record `(raw_estimate, provider_logical_prompt_tokens)` for successful requests and maintain a conservative per-model EMA. Apply that factor to the *current* raw estimate; never substitute the prior measurement directly. This preserves the Round-3 fix while learning how badly bytes/4 underestimates a particular local model.

Second, reduced tool output is durably archived, but the model cannot actually read that archive. The generic reduction notice currently says the full output is archived and suggests reading it “by path,” even though `exec`/`process` output has no model-readable path and the built-in tools contain no blob/payload reader. Pi's shell path instead exposes a real temp-file location for truncated output. I would eventually add a bounded read-only `tool_output`/`payload_read` capability keyed by an opaque recovery reference, rather than exposing internal blob-store paths. That would make Rupi's excellent durable recovery mechanism useful to the working model as well.

## Recommended Round-4 implementation slice

For the next PR, I would keep the scope concentrated. The best order is:

1. **Full pre-execution argument validation**, especially optional fields and `additionalProperties`.
2. **Make progress boundary a real postcondition**, with `ToolChoice::Required` as an assistive provider hint but runtime enforcement as the authority.
3. **Add replay-safety metadata to model failures** and eliminate fake same-adapter retries after quarantine.
4. **Expose the existing OpenAI-compat dialect settings through `ModelEndpoint`**, including explicit thinking-off behavior.
5. Add **strict/preferred constrained tool sampling** on top of the new dialect/capability contract.
6. Add **one bounded output-truncation recovery attempt**.
7. Refactor **context overrides** into the active-window/adaptive evaluation path.

After that PR, I would run deterministic fault-injection tests before returning to the task benchmark. In particular, I want to see a deliberately “bad local model” fixture emitting wrong optional argument types, ignoring a required progress tool, malformed JSON, a post-POST timeout, and a `length` stop. If Rupi handles those coherently, **then cases 01/02/04/06 become much more informative**.

The overall trajectory is encouraging: Round 1 was about missing mechanisms; Round 2 about mechanism consistency; Round 3 about lifecycle invariants. Round 4 is now largely about making the harness **actively compensate for weak model/provider behavior instead of merely surviving it**. I would not claim Pi parity yet, but the remaining gap is becoming much more localized.
