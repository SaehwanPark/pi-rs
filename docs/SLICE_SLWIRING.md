# Slice: the interactive loop draws its status line from `statusline::line`

`crates/pi-rs-tui/src/statusline.rs` is the projection (snapshot in, `RenderLine` out — read its
doc comment and its `Status`/`Activity` fields first; do not read the whole file).
`src/interactive.rs` is the loop that should be drawing it but currently builds its own line.

## Obligations
1. `grep -n "status_line\\|Status\\|columns\\|set_width" src/interactive.rs` to find what the loop
   draws today and where it gets the terminal width from. The status line must use **the same width
   source the editor already uses** — two different width notions in one frame is a bug.
2. Replace the hand-built line with `pi_rs_tui::statusline::line(&Status { .. })` and render the
   returned `RenderLine` through the path the loop already uses to paint, so the existing
   erase/repaint behaviour from PR #23 is preserved (it fixed a real bug: the erase walked past the
   top of the frame). Do not re-implement erase logic.
3. `Activity::Running` for the whole duration a turn is being executed, `Activity::Waiting` while
   the loop waits for input. `turns` = completed turns so far. Model name comes from the session:
   `SessionHandle` in `src/run.rs` no longer exposes it (the accessor was deleted as unused), so
   re-add a minimal `model()` accessor and use it.
4. **A cancelled turn leaves the line showing `Waiting`.** The loop, not the status line, reports
   the cancellation; the status line must not stay stuck on `Running`.
5. What the line says must not regress: whatever the current line shows that is still true keeps
   showing (same wording where it already reads well, `1 turn` singular comes from `statusline`).

## Constraints
- `pi-rs-tui` stays free of store/provider/runtime imports; this slice touches `src/interactive.rs`,
  `src/run.rs` (accessor only), tests, ROADMAP/IMPLEMENTATION_STATUS annotations. 2-space, 100 cols.
- No new dependency. Do not change `statusline.rs` unless a test proves it wrong — say so if you do.
- `bash bench/startup.sh` must not move more than noise.
- Keep/extend the interactive tests in their existing style (they drive the loop with a non-tty
  surface; `grep -n "mod tests" -A40 src/interactive.rs` for the shape). Cover at least: the drawn
  line contains the model name and `idle`/`waiting` wording before any turn; the line is drawn from
  `statusline::line` rather than a literal (assert on content the projection produces); after a
  cancelled turn the loop is back to the waiting wording.

## Verify (paste real numbers)
`cargo fmt --all --check` · `cargo clippy --workspace --all-targets -- -D warnings` ·
`cargo test --workspace` (baseline on this branch: **567 passed**) · `bash bench/startup.sh` ·
if you can drive it, one real pty run: `printf 'hello\\n' | timeout 20 script -qec 'cargo run -q --
interactive --store /tmp/slwiring-store' /dev/null` (or explain why not).

Commit `--allow-empty -m "chore: start"` first, commit after every step, always compiling. Do not
merge, rebase, push, open PRs, or touch other branches or /home/saehwan/repos/pi-rs.

---

## Context discipline (this is why three sibling slices died at once)

Three children were launched simultaneously on the shared local model server, and all three died with
the server-side error *"Context size has been exceeded"*. The server was healthy; four concurrent
~100k sessions (three children + the parent) exceeded what it will serve. You are the **only** child
running now. Behave accordingly:

- Never grep repo-wide. Grep the **one** file you need, with `| head -15`.
- Read at most ~40 lines per `read`; use `offset`. Whole-file reads of `src/interactive.rs` or
  `src/run.rs` will end this run.
- Keep any single command's output under 15 lines.
- Commit after every step. If you die, the last compiling commit is the deliverable — that is a fine
  outcome, not a failure to hide.
