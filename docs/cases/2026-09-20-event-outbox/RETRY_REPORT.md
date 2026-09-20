# Bounded retry report: output-limited completion

Date: 2026-09-20
Branch: `tester/2026-09-20-loop-2-live-case`
Parent runtime fix: `289262f` (`fix: reject output-limited model completions`)
Model endpoint: `http://127.0.0.1:8000/v1` (`qwen3.8-flash-next`)

This was a stopgate retry only. It did not ask the model to implement or edit
Event Outbox. The probe used a one-request turn and an eight-token endpoint
output cap.

## Endpoint and binary readiness

Exact endpoint check from the repository root:

```powershell
$sw=[Diagnostics.Stopwatch]::StartNew(); try { $r=Invoke-WebRequest -UseBasicParsing -Uri 'http://127.0.0.1:8000/v1/models' -TimeoutSec 10; $code=[int]$r.StatusCode; $body=$r.Content } catch { $code=0; $body=$_.Exception.Message }; $sw.Stop(); "status=$code elapsed_ms=$($sw.ElapsedMilliseconds)"; $body
```

Observed: HTTP `200`, `84 ms`; the response listed `qwen3.8-flash-next`.

The parent fix was built before the live probe:

```powershell
$sw=[Diagnostics.Stopwatch]::StartNew(); cargo build --bin rupi; $code=$LASTEXITCODE; $sw.Stop(); "cargo_build_exit=$code elapsed_ms=$($sw.ElapsedMilliseconds)"; exit $code
```

Observed: exit `0`, `6846 ms` (`Finished` reported `6.75s`).

The committed probe config is
`project/rupi.retry.config.json`. Its relevant bounds are:

```text
thinking=off
max_model_requests_per_turn=1
capabilities.max_output_tokens=8
auto_approve_mutating=true
```

## Live provider probe

Working directory:
`docs/cases/2026-09-20-event-outbox/project`

Exact command:

```powershell
$prompt = 'Provide a detailed 1000-word explanation of why durable event outboxes use idempotency keys and retry state. Do not use tools; answer in plain text.'; $sw=[Diagnostics.Stopwatch]::StartNew(); & 'C:\Users\saehwan\repos\pi-rs\target\debug\rupi.exe' run --config rupi.retry.config.json --cwd . --prompt $prompt --no-color --no-reasoning --verbose; $code=$LASTEXITCODE; $sw.Stop(); "rupi_exit=$code elapsed_ms=$($sw.ElapsedMilliseconds)"; exit $code
```

Observed stdout/stderr and status:

```text
[request] local/qwen3.8-flash-next
[model] length · in 2898 · out 8 · 6.3 s · reasoning: reasoning
[warn] model request failed (semantic): provider stopped at its output limit before completing the response
[turn] semantic · 6.3 s
[session end] interrupted · provider failure: semantic: provider stopped at its output limit before completing the response
error: provider failure: semantic: provider stopped at its output limit before completing the response
rupi_exit=1 elapsed_ms=6662
```

This is a reproducible live provider result: the deliberate cap caused
`finish_reason=length`, the CLI exited nonzero, and the turn was failed as a
semantic incomplete response. The probe made one model request and did not
produce a final answer or invoke a tool.

Session: `01a0bfcc-09d8-7700-adaf-373dd5da4f92`
Trace:
`.rupi-state-retry/sessions/01a0bfcc-09d8-7700-adaf-373dd5da4f92.trace.jsonl`

The independent trace fields were:

```text
model_request_completed: finish_reason=length, input_tokens=2898, output_tokens=8, duration_ms=6313, tool_calls=0
turn_completed: status.failed.kind=semantic, duration_ms=6341
diagnostic: model request failed (semantic): provider stopped at its output limit before completing the response
session_ended: reason.interrupted.message=provider failure: semantic: provider stopped at its output limit before completing the response
event counts: model_request_started=1, model_request_completed=1, model_retry=0, model_failover=0, turn_completed=1, session_ended=1
```

There are no `model_retry` or `model_failover` events in the trace. The
one-request configuration also bounds the live probe; the runtime-level
no-retry assertion is covered by the focused test below.

## Focused Rust evidence

These tests were run against the parent fix, each with exit `0`:

```powershell
cargo test -p rupi-core output_limit_finish_reasons_are_not_usable_answers --lib
```

`1 passed; 0 failed; 95 filtered out; finished in 0.00s`; wrapper elapsed
`135 ms`.

```powershell
cargo test -p rupi-provider output_limit_finish_reason_survives_for_runtime_classification --lib
```

`1 passed; 0 failed; 74 filtered out; finished in 0.00s`; wrapper elapsed
`161 ms`.

```powershell
cargo test -p rupi-runtime output_limited_completion_is_failed_and_keeps_provider_evidence --lib
```

`1 passed; 0 failed; 89 filtered out; finished in 0.00s`; wrapper elapsed
`160 ms`.

The runtime test independently asserts semantic failure, zero `model_retry`
events, preserved `finish_reason=length`, and preserved output-token count.

## Trace and replay

Exact trace command:

```powershell
& 'C:\Users\saehwan\repos\pi-rs\target\debug\rupi.exe' trace --config rupi.retry.config.json 01a0bfcc-09d8-7700-adaf-373dd5da4f92 --no-reasoning --no-color
```

Result: `trace: 01a0bfcc-09d8-7700-adaf-373dd5da4f92 · 16 entries read · 8 shown`,
exit `0`, elapsed `17 ms`. It displayed the `length` model boundary, semantic
warning, failed turn, and interrupted session end.

Exact replay command:

```powershell
& 'C:\Users\saehwan\repos\pi-rs\target\debug\rupi.exe' replay ".rupi-state-retry\sessions\01a0bfcc-09d8-7700-adaf-373dd5da4f92.trace.jsonl" --tools --sequence
```

Result: exit `0`, elapsed `17 ms`. No historical tool execution was performed.

## Regression checks

The already-complete project suite was rerun from `project/`:

```powershell
$sw=[Diagnostics.Stopwatch]::StartNew(); python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v; $code=$LASTEXITCODE; $sw.Stop(); "suite_exit=$code elapsed_ms=$($sw.ElapsedMilliseconds)"; exit $code
```

Result: `Ran 6 tests in 0.363s`, `OK`, exit `0`, wrapper elapsed `466 ms`.

The independent fresh-process oracle was rerun from the repository root:

```powershell
$sw=[Diagnostics.Stopwatch]::StartNew(); python -W error::ResourceWarning -m unittest discover -s docs/cases/2026-09-20-event-outbox/acceptance -p "test_*.py" -v; $code=$LASTEXITCODE; $sw.Stop(); "oracle_exit=$code elapsed_ms=$($sw.ElapsedMilliseconds)"; exit $code
```

Result: `Ran 2 tests in 2.601s`, `OK`, exit `0`, wrapper elapsed `2720 ms`.

## Remaining friction and issue status

1. **Medium:** Even with an eight-token cap, the local Qwen request took
   `6313 ms` and used native reasoning for all eight output tokens; the first
   delta arrived after `1864 ms`. This is a latency/progress concern, not a
   completion-classification failure.
2. **Low:** The compact `trace` view does not show token counts; inspecting the
   JSONL trace is required to independently read `input_tokens=2898` and
   `output_tokens=8`.

The output-limit runtime issue from the original Event Outbox report is
resolved by `289262f` and is live-proven here. A broader issue remains from the
original case: an unconstrained local Qwen implementation turn can spend many
minutes before producing useful writes. This bounded retry does not claim that
progress/latency issue is fixed.

## Final total-deadline retry

Parent runtime commit: `20d6218` (`feat: add optional provider request deadlines`).
This was the final bounded retry; no implementation model turn was attempted and
the Event Outbox project and acceptance oracle were not changed.

The local endpoint was checked again from the repository root with the same
`/v1/models` command recorded above. It returned HTTP `200` in `73 ms` and
listed `qwen3.8-flash-next`.

The parent binary was rebuilt before the probe:

```powershell
$sw=[Diagnostics.Stopwatch]::StartNew(); cargo build --bin rupi; $code=$LASTEXITCODE; $sw.Stop(); "cargo_build_exit=$code elapsed_ms=$($sw.ElapsedMilliseconds)"; exit $code
```

Result: exit `0`, wrapper elapsed `6604 ms` (`Finished` reported `6.52s`).

The checked-in case-local config is
`project/rupi.deadline.retry.config.json`:

```text
thinking=off
max_model_requests_per_turn=1
request_timeout_ms=1000
capabilities.max_output_tokens=32768
```

The prompt was deliberately plain text and asked for a 4,000-word essay, which
would not complete within a one-second request deadline. Exact command:

```powershell
$prompt = 'Write a 4000-word plain-text essay explaining durable event outboxes, idempotency keys, retry state, and restart recovery. Do not use tools and do not emit structured data; respond only with prose.'; $sw=[Diagnostics.Stopwatch]::StartNew(); & 'C:\Users\saehwan\repos\pi-rs\target\debug\rupi.exe' run --config rupi.deadline.retry.config.json --cwd . --prompt $prompt --no-color --no-reasoning --verbose; $code=$LASTEXITCODE; $sw.Stop(); "rupi_exit=$code elapsed_ms=$($sw.ElapsedMilliseconds)"; exit $code
```

Live Qwen output and status:

```text
[model] no finish reason · 3.1 s
[warn] model request failed (timeout): provider request exceeded its configured total timeout (1000 ms)
[retry] 2 of 2 · timeout
[request] local/qwen3.8-flash-next
[model] no finish reason · 0 ms
[warn] model request failed (provider_unavailable): provider adapter is quarantined after an abandoned request
[turn] provider_unavailable · 3.2 s
[session end] interrupted · provider failure: provider_unavailable: provider adapter is quarantined after an abandoned request
error: provider failure: provider_unavailable: provider adapter is quarantined after an abandoned request
rupi_exit=1 elapsed_ms=3544
```

This live run proves that the endpoint request reached the configured timeout
classification and exited nonzero, but it does **not** prove the requested
absence of retry or a hard one-second wall-clock bound. The runtime emitted one
automatic retry after the timeout even with
`max_model_requests_per_turn=1`; the retry immediately encountered the
quarantined adapter. No model failover occurred.

Session: `01a0bfdb-4c9b-74c6-a735-36e0727c3abe`
Trace:
`.rupi-state-deadline-retry/sessions/01a0bfdb-4c9b-74c6-a735-36e0727c3abe.trace.jsonl`

Independent JSONL evidence:

```text
model_request_started=2
model_request_completed=2
model_retry=1
model_failover=0
diagnostic=2
turn_completed=1
session_ended=1
first model_request_completed: duration_ms=3102, finish_reason absent, input/output token counts absent, tool_calls=0
model_retry: attempt=2, kind=timeout, max_attempts=2, will_failover=false
second model_request_completed: duration_ms=0
turn_completed: status.failed.kind=provider_unavailable, duration_ms=3224
session_ended: interrupted; provider adapter is quarantined after an abandoned request
```

The first request's recorded `duration_ms=3102` and the CLI wrapper's
`3544 ms` exceed the configured `1000 ms` deadline. The timeout was detected and
typed, but cancellation/quarantine did not make the in-flight adapter or the
CLI return within that deadline.

Focused parent-fix checks also passed:

```powershell
cargo test -p rupi-core endpoint_timeout_overrides_round_trip_and_reject_zero --lib
```

`1 passed; 0 failed; 95 filtered out; finished in 0.00s`; wrapper elapsed
`135 ms`, exit `0`.

```powershell
cargo test -p rupi-provider zero_request_timeout_is_rejected --lib
```

`1 passed; 0 failed; 75 filtered out; finished in 0.00s`; wrapper elapsed
`160 ms`, exit `0`.

```powershell
cargo test -p rupi-provider total_request_deadline_is_distinct_from_idle_timeout --test transport
```

`1 passed; 0 failed; 21 filtered out; finished in 2.31s`; wrapper elapsed
`2458 ms`, exit `0`.

The focused transport test confirms typed timeout and adapter quarantine in its
controlled fixture; the live Qwen result above is the independent evidence for
the actual endpoint path and its wall-clock behavior.

Trace command:

```powershell
& 'C:\Users\saehwan\repos\pi-rs\target\debug\rupi.exe' trace --config rupi.deadline.retry.config.json 01a0bfdb-4c9b-74c6-a735-36e0727c3abe --no-reasoning --no-color
```

Result: `12 entries read · 12 shown`, exit `0`, elapsed `20 ms`.

Replay command:

```powershell
& 'C:\Users\saehwan\repos\pi-rs\target\debug\rupi.exe' replay ".rupi-state-deadline-retry\sessions\01a0bfdb-4c9b-74c6-a735-36e0727c3abe.trace.jsonl" --tools --sequence
```

Result: exit `0`, elapsed `19 ms`; no historical tool execution was performed.

The Event Outbox project suite was rerun from `project/`:

```powershell
$sw=[Diagnostics.Stopwatch]::StartNew(); python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v; $code=$LASTEXITCODE; $sw.Stop(); "suite_exit=$code elapsed_ms=$($sw.ElapsedMilliseconds)"; exit $code
```

Result: `Ran 6 tests in 0.410s`, `OK`, exit `0`, wrapper elapsed `533 ms`.

The independent fresh-process oracle was rerun from the repository root:

```powershell
$sw=[Diagnostics.Stopwatch]::StartNew(); python -W error::ResourceWarning -m unittest discover -s docs/cases/2026-09-20-event-outbox/acceptance -p "test_*.py" -v; $code=$LASTEXITCODE; $sw.Stop(); "oracle_exit=$code elapsed_ms=$($sw.ElapsedMilliseconds)"; exit $code
```

Result: `Ran 2 tests in 2.718s`, `OK`, exit `0`, wrapper elapsed `2834 ms`.

## Final ranked issue status

1. **High — the total deadline is not a hard wall-clock bound at the CLI/request
   boundary.** A configured `1000 ms` deadline produced a typed timeout, but the
   first request was recorded as `3102 ms` and the CLI took `3544 ms`. The
   in-flight adapter was quarantined, yet joining/cancelling it did not complete
   within the configured deadline.
2. **High — timeout recovery retries despite the one-request case bound.** The
   live trace records `model_retry=1` (`attempt=2`, `max_attempts=2`) after the
   timeout. The retry did not reach Qwen because the adapter was quarantined, but
   it changed the final diagnostic to `provider_unavailable`.
3. **Medium — final status obscures the root timeout.** The first diagnostic is
   the useful typed `timeout`, while `turn_completed` and `session_ended` report
   the subsequent quarantine failure.

Therefore the previously high unbounded-turn friction is **not fully addressed**
for the documented bounded workflow. The deadline prevents indefinite semantic
waiting in the sense that a timeout is detected, but this live reproduction
leaves two major runtime issues: the in-flight request and CLI exceed the
configured deadline, and automatic timeout retry is still visible. The Event
Outbox implementation and independent acceptance remain green.
