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
