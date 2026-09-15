# Slice: let the tests decide the wiring (no review, no rewrite)

A previous run died twice on context while trying to *review* ~295 lines of highlight wiring in
`src/interactive.rs`. Stop reviewing. Write the invariants as tests and let them pass or fail.

## Do exactly this

1. `grep -n "highlight\|fn .*highlighted\|fn .*render\|Role::" src/interactive.rs | head -20` to find the
   function(s) the wiring added. Read at most 40 lines around one of them.
2. Add a table-driven test **in the existing test module of `src/interactive.rs`** asserting, for each
   input: the styled path's plain text equals the input, and its rendered display width equals
   `pi_rs_tui::width::display_width(input)` (use whatever width helper the file already uses — grep for
   it, do not add one). Table must include: `"status"`, `"status --all"`, a double-quoted argument with
   a space, an unterminated quote `"unterminated`, a CJK argument, an emoji argument, leading whitespace.
3. Add one test that the no-colour path yields the same characters as the plain path.
4. Commit the tests with the **observed** result in the message, whether green or red, before doing
   anything else. If red, stop and report the failing assertions verbatim (≤15 lines). Do **not** modify
   any non-test code. Do not delete or "simplify" the wiring.

## Limits

At most **three** `read` calls, ≤40 lines each; one grep per file `| head -20`; output ≤15 lines;
`cargo test --bin pi-rs` only, never `--workspace`. Commit before running cargo. No merges, rebases,
pushes, PRs; stay in this worktree. `git rev-parse HEAD` before each commit — if it moved without your
commit, stop and report.
