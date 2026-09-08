# Slice: render the classifier (wiring only, no classifier changes)

`pi_rs_tui::highlight::tokens` exists (#39, cherry-picked here). Wire it where the interactive loop
actually draws an input line. **Do not change `highlight.rs`** except to fix a real bug you can name.

## Find the render site, do not invent one

`grep -n "prompt\|input\|echo\|render\|push_str" src/interactive.rs | head -20` then read ≤40 lines
around the one hit that draws user-entered text. If the loop only *echoes* submitted input and never
renders a live input line, wire the echo and say that plainly in the PR — do not build a live input
widget, that is a different slice.

## Invariants (these are the whole review)

1. **Highlighting must not change what is on screen, only how it is coloured.** The rendered plain text
   must equal the input, and the display width must be unchanged — assert both.
2. Use the existing `line::{RenderLine, Segment}` and `style::{Palette, Role}` APIs and the existing
   width helpers. Add no dependency, no new role, no new surface format.
3. Colour is **additive**: if the palette or terminal declines colour, the output must degrade to exactly
   what it renders today. Do not restructure the existing draw path beyond what segmentation requires.
4. `tokens` does **not** reproduce separators (its doc comment says so), so do not reconstruct the line
   from segment texts — keep the original string for the plain/width path and use segments for styling.
   If that forces an overlap or an off-by-one in column offsets, stop and report instead of guessing.

## Tests

* table-driven: for each input, the styled path's plain text equals the input and its display width
  equals `width::display_width(input)` — include CJK and an emoji, plus an unterminated quote;
* the no-colour path renders the same characters as before the change;
* existing interactive tests still pass unmodified.

## Limits

At most **four** `read` calls, ≤40 lines each; one `grep -n` per file `| head -20`; output ≤15 lines;
`cargo test --workspace` once at the end, `cargo test -p pi-rs-tui` / `--bin pi-rs` in between.
**Commit before running any cargo command.** Never weaken an assertion; a named gap beats a fake test.
Leave ROADMAP unchecked. No merges, rebases, pushes, PRs; stay in this worktree. `git rev-parse HEAD`
before each commit — if it moved without your commit, stop and report.
