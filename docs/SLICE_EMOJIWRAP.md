# Slice: wrapping must not cut a joined emoji (the red test #41 exposes)

`feat/highlight-wrap` leaves one test active and red, with the cause already traced:

> `row 0 of "👨‍👩‍👧 ship it now" at 6 ends on a joiner: "👨‍👩‍👨‍👩‍👧"`

Breaks are chosen in `line_rows` (`crates/pi-rs-tui/src/editor.rs:748`): it is display-width aware but
breaks **between `char`s**, so a cluster whose tail overflows starts the next row. `segment_row` /
`input_rows` only re-segment rows they are handed — that is why the previous slice correctly refused to
"fix" it there.

## Contract

`line_rows` must not place a break *inside* a character cluster. Minimal rule, **no new dependencies**
(`unicode-segmentation` is not available and must not be added for this):

* never break immediately **before** or **after** U+200D (ZERO WIDTH JOINER);
* never break immediately before U+FE0E / U+FE0F (variation selectors), U+20E3 (combining enclosing
  keycap), any char in the combining ranges `0300..=036F`, or U+200C;
* never break between a regional indicator and its pair (U+1F1E6..=U+1F1FF pairs) — flag pairs are two
  chars, one cluster.

Do **not** hand-implement full UAX #29 grapheme clustering. State the rule and its limits in the doc
comment: this covers emoji sequences, combining marks and keycaps; it does not claim full Unicode
grapheme parity.

## Invariants that must not regress (these are the risk)

`line_rows` feeds the cursor walk and the erase count, which assume **one segment per drawn row**.
* Do not merge rows. * Do not change row count for pure-ASCII input — assert it in a test that compares
  wrapping of ASCII lines before and after your change (same inputs, same rows).
  * Column accounting must stay display-width based: a row that fits before still fits.

## Tests

* the existing red test (`a_joined_emoji_is_not_broken_across_a_wrapped_row`) turns green;
* a flag pair (`🇰🇷` at width 1 and 2) is never split;
* a combining mark (`e\u{0301}`) is never split;
* ASCII wrapping unchanged (explicit same-rows assertion);
* CJK at odd widths still never splits a double-width char.

## Limits

Three `read` calls, ≤40 lines each; one grep per file `| head -20`; output ≤15 lines;
`cargo test -p pi-rs-tui` then `cargo test --bin pi-rs`, `--workspace` once at the end. **Commit before
running cargo.** Never weaken an assertion. No new dependencies — if you think one is unavoidable, stop
and report instead. Leave ROADMAP unchecked. No merges, rebases, pushes, PRs; stay in this worktree.
`git rev-parse HEAD` before each commit — if it moved without your commit, stop and report.
