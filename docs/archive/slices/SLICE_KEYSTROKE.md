# Slice: measure keystroke latency, and put the budgets where a regression trips

Phase 1 asks to "measure keystroke/render latency". Rendering and command parsing are covered by
`bench/render.sh` + `crates/pi-rs-tui/benches/render.rs` on the branch `bench/tui-render-budgets`
(PR #15, open). **Do not merge that branch here.** Read its shape for reference:

    git show origin/bench/tui-render-budgets:bench/render.sh
    git show origin/bench/tui-render-budgets:crates/pi-rs-tui/benches/render.rs

Mirror that shape exactly: `harness = false`, std timing only, the case list and the budgets in the
bench source, non-zero exit when a case exceeds its budget, and a thin `bench/keystroke.sh` wrapper
with the same interface as `bench/render.sh` (`--iterations`, `--json`). No new dependency.

## Cases to measure (keystroke = one `Editor::apply`, plus the redraw it forces)
- insert a character at the end; insert at the middle of a paragraph
- backspace; delete-word
- `word-left` / `word-right` on a long line
- history recall up/down with a 50-entry history
- `apply` on a 2000-character single-line buffer (held-down key, no wrap help)
- `display(prefix)` after each keystroke at width 80 on that 2000-char buffer — this is the
  keystroke+redraw pair the roadmap item names, and the one that would feel like lag
- `display(prefix)` at width 40 on a 50-line buffer
- a scripted 1000-keystroke session cycling insert / backspace / word-left / word-right, timed per
  keystroke (the realistic mixed case)

## Budget rule (put it in the source, with the reason)
Run each case, take the median, set the budget to **median x 10** with a floor of 2 µs, and keep
every per-keystroke budget under **1 ms**: terminal key repeat is ~30 ms, so a keystroke that costs
a millisecond is already late, and a 10x margin is what survives a loaded CI runner without turning
the bench into a coin flip. Report the medians you measured and the budgets you wrote.

## Constraints
- Bench only. Nothing here runs in `cargo test`; `cargo clippy --workspace --all-targets` compiles
  benches, so it must stay warning-free.
- `bench/startup.sh` must not move (this is not startup path).
- 2-space indent, 100 columns, edition 2024.
- `crates/pi-rs-tui/Cargo.toml`: add a second `[[bench]]` block. Note in your report that this file
  will need a take-both merge with PR #15, which adds its own `[[bench]]` block.

## Verify (paste real numbers)
`cargo fmt --all --check` · `cargo clippy --workspace --all-targets -- -D warnings` ·
`cargo test --workspace` · `bash bench/keystroke.sh` (must exit 0 against your own budgets) ·
`bash bench/startup.sh`

Commit `--allow-empty -m "chore: start"` first, then commit after every step, always compiling.
Tick the ROADMAP "Measure keystroke/render latency" item ONLY if you judged keystroke and render
both covered; otherwise annotate it like the status-line item was. Do not merge, rebase, push, open
PRs, or touch other branches.
