# Handoff: auto-summarize on overflow

Branch: `feat/auto-summarize-on-overflow` (pushed, WIP commit `wip: summarize
history when a request crosses the window`). Main is clean and green at
`e3930ce` (842 workspace tests, clippy/fmt clean). Two tests fail on the
branch. This document is the resumption contract.

## Objective

When a request crosses the provider's context window — either measured by
`build_request` or refused by the provider with `ContextOverflow` — the turn
loop should summarize the oldest history once, re-issue the request, and
continue, instead of ending the turn. One summarization per turn. `/compact`
and the overflow intercept must share the same summarization contract (the
first PR on this branch opened the durable epoch; this slice connects it).

## What is already in place (on the branch)

- `maintain(&mut self, refusal, provider_crossed, turn_id, cancel, progress,
  report, clock)` — the single intercept, called from the
  `TurnFailure::Aborted(TurnError::Aborted(_))` arm with `self.provider_crossed`.
- `summarize_oldest(oldest)` — asks the model for the summary of the oldest
  prefix, with a stand-in placeholder sized to `summary_reserve` so the ask is
  measured exactly as the re-issue will carry it; returns
  `Result<bool, TurnError>` (`false` when the instruction cannot fit).
- `compact_with(summary, retained, summary_position, stand_in)` — maintenance
  inserts the summary at the measured position; `/compact` keeps its
  prepend-at-0 behavior through `compact()`.
- `summarized_candidate()` — builds the candidate history; keeps an already
  written summary (first message contains `SUMMARY_MARK`) instead of adding a
  placeholder.
- Scan direction: provider-crossed → smallest fitting tail first
  `(1..=total).find(fits)`; measurement-refused → largest worthwhile tail
  `(1..=total).rev().find(fits && worth)`.
- Budget guard: `spare < 2` → finish `Failed{ContextOverflow}` with
  `budget_exhausted=true` instead of spending the summarization request.
- `TurnError::Exhausted`, `TurnFailure::Aborted(TurnError)`, `overflow_seen`,
  `turn_question` (question excluded from the summarized prefix),
  `is_summary_request()` (the `Scripted`-style refusals are answered with the
  summary ask instead of being swallowed), `SUMMARY_MARK` /
  `SUMMARY_INSTRUCTION` / `SUMMARY_PREAMBLE` consts, `summary_reserve =
  window/4`.
- Build-request preflight in the retry loop was REMOVED: calling
  `build_request()` as a probe was mutating history (the
  `ContextAction::ReducePayload` arm evicts), so maintenance measured a
  one-message history instead of six. `attempt()` is the only caller.

## Why the two tests fail (root cause, confirmed)

Both overflow fixtures use `context_window = 8` with 1-token filler messages
(`"xxxx"`; `estimate_messages` is `bytes/4`). `SUMMARY_INSTRUCTION` is ~65
tokens by itself. `summarize_oldest` correctly refuses the ask (65 > 8) and
returns `Ok(false)` — summarization is *structurally impossible* at that
window. The intercept then ends the turn with the original refusal, which is
the right behavior for an impossible window and the wrong ending for these
fixtures. Debug runs confirmed: `MAINT total=1` was the preflight bug (fixed);
what remains is purely the window size.

## How to finish

1. Drop the debug `eprintln!`s first — `grep -n eprintln crates/pi-rs-runtime/src/turn.rs`
   (including the `DIAG*/KIND1` sinks in the test module).
2. Raise the two fixtures to a window that can hold instruction + stand-in +
   tail (≥ 128 is comfortable; keep `RefusingOnce` refusing until the request
   exceeds the window, and keep the answer scripted after maintenance).
   Recompute the filler count so the first request still crosses.
3. Expected end state: `an_overflowed…` completes with `requests == 3`
   (refused, summary ask, re-issue); `a_second_refusal…` ends `Failed` after
   exactly one summarization with epoch count 1.
4. `cargo test -p pi-rs-runtime`, then workspace `cargo test`, `cargo clippy`,
   `cargo fmt --check`, `bash bench/*.sh` if the turn path changed timings.
5. Open the PR against main; description should lead with the behavior, not
   the plumbing.

## Verification commands

```bash
cargo test -p pi-rs-runtime
cargo test          # workspace
cargo clippy -- -D warnings
cargo fmt --check
```

Do not use `cargo bench`; use `bash bench/*.sh`.

## Backlog after this slice (from ROADMAP / prior sweep)

- `--resume <text>` fuzzy session matching; `--extension` flag.
- `/skill` TUI wiring; thinking-level capability probe; status-bar context meter.
- Two compaction designs still coexist (`SessionCompactionRecord` vs
  `ContextCompactionEpoch`) — reconcile deliberately, in its own slice.
- Provider `resolve_model` failures are mislabeled "authentication required"
  in `resolve_for_run` (~line 195); mislabeling costs users hours.
