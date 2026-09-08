# Slice: painted input wrapping at narrow widths (the gap #40 named)

PR #40 pins single-row painting: `paint(line).plain() == line`, width equals `display_width(line)`,
monochrome identical. It does **not** cover wrapping — every test line fits one row. `segment_row` and
`input_rows` in `src/interactive.rs` are where wrapping happens. Close the gap.

## Tests first, and they are the deliverable

Table-driven, in the existing `mod tests` of `src/interactive.rs`, over inputs that cannot fit:

* an argument longer than the column count;
* a CJK-heavy line (width 2 per char) at a width that is odd, so a cluster cannot be split in peace;
* an emoji argument;
* an unterminated quote long enough to wrap;
* a line that wraps more than twice.

For each, assert the three properties that must survive wrapping:

1. the visible text across all rows, with wrapping-induced breaks removed, equals the input;
2. **every** row's display width is ≤ the column count, and no row is empty unless the input was;
3. no grapheme cluster or double-width character is split — assert by re-composing the row text and
   comparing `display_width` before and after.

Also assert colour is still additive under wrapping: monochrome rows == plain rows, no `ESC`.

## Fixing, if a property fails

Make the **smallest** fix in `segment_row` / `input_rows` that restores the property, and write in the
commit message exactly what you changed and which assertion forced it. If a property is unsatisfiable
without redesigning wrapping, stop: commit the failing tests (they are still worth having, marked
`#[ignore]` with the reason in the attribute only if you must keep CI green — prefer leaving them active
and reporting red to me instead).

## Limits

Three `read` calls, ≤40 lines each; one grep per file `| head -20`; command output ≤15 lines;
`cargo test --bin pi-rs` only. **Commit before running cargo.** Never weaken an assertion. Leave ROADMAP
unchecked. No merges, rebases, pushes, PRs; stay in this worktree. `git rev-parse HEAD` before each
commit — if it moved without your commit, stop and report.
