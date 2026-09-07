# Slice (retry, bounded): `status_line` delegates to `statusline::line`

The previous run of this slice explored the repo and died with nothing committed. This one is
mechanical. **Do only what is written here. Do not search the repo beyond the greps given.**

## Known facts (verified by the parent; do not re-derive)
- `src/interactive.rs` is 557 lines. `fn status_line(model: &str, state: TurnState, columns: usize) -> String`
  is at line **140**. Call sites: lines **293** and **308**, both `status_line(&self.model, self.state, self.columns)`.
  Tests: lines **488-509** (`the_status_line_names_the_model_and_the_turn`,
  `the_status_line_never_spills_onto_a_second_row`).
- Projection API (`crates/pi-rs-tui/src/statusline.rs`): `Status { model, activity, turns, columns, hint }`,
  `Activity::Waiting | Activity::Running`, `statusline::line(&Status) -> RenderLine`, and `RenderLine::plain() -> String`.

## Required reads, and nothing else
1. `src/interactive.rs` with `offset=118, limit=50` — the function and what is around it.
2. `src/interactive.rs` with `offset=484, limit=32` — the two tests.
3. `crates/pi-rs-tui/src/statusline.rs` with `offset=1, limit=70` — field names and `Activity` variants.
4. To find a turn counter: `grep -n "turns\|TurnState::Working" src/interactive.rs | head -12`.

## The change
1. Rewrite `status_line` so its body builds `pi_rs_tui::statusline::Status { .. }` and returns the
   projected plain text. Mapping: `TurnState::Idle -> Activity::Waiting`,
   `TurnState::Working -> Activity::Running`. Keep the signature `-> String`.
2. `turns`: use an existing counter if the grep finds one on the same struct; otherwise add
   `turns: usize` to that struct, start it at 0, and increment where a turn finishes.
3. **The loop's no-spill invariant wins.** The projection's floor at `columns: 0` is `"idle"`
   (non-empty); the loop must never emit more than `columns` wide. So if `line.plain()` is wider
   than `columns`, truncate with `pi_rs_tui::width::truncate`. Keep the existing test's meaning:
   at `columns: 0` the loop yields `""`.
4. Update the two tests only where the projected *wording* differs, keeping what each asserts about
   behaviour. Add one test: the string comes from the projection — assert the separator form the
   projection produces (read it in step 3), not a hand-written copy of it.

## Discipline
- 2-space indent, 100 columns. `cargo fmt --all` after each edit.
- Every command output under 15 lines (`| head`). Every `read` bounded by `offset`/`limit`.
- Commit after each numbered step, message prefixed `refactor(tui):`. Always compiling.
- No merges, rebases, pushes, PRs. Stay in this worktree.

## Verify (paste, keep it short)
`cargo fmt --all --check` · `cargo clippy --workspace --all-targets -- -D warnings` ·
`cargo test --workspace 2>&1 | grep "^test result"` (baseline **567**) · `bash bench/startup.sh`
