# Slice: a compact status line, as a pure projection

`pi-rs-tui` has a `Role::Status` in its palette and no module that produces one. This slice adds
that module: a snapshot in, a `RenderLine` out. No terminal, no runtime, no state.

## Existing API to build on (do not reinvent)
`crates/pi-rs-tui/src/line.rs`: `RenderLine::new()`, `push(text, role)`, `width()`, `plain()`,
`render(palette)`, `wrapped(width, NarrowDecoration)`.
`crates/pi-rs-tui/src/style.rs`: roles already exist — `Role::Meta` ("machine facts that are not
content: counts, durations, ids"), `Role::Status` ("the status line"), `Role::Muted` ("quiet
context that should recede"), `Role::Warning`. Use these; do not add a role.
`crates/pi-rs-tui/src/width.rs`: `MIN_COLUMN`, `display_width`, `fits`, `truncate`.

## Contract
```rust
pub enum Activity { Waiting, Running }
pub struct Status<'a> { pub model: &'a str, pub activity: Activity, pub turns: usize, pub columns: usize }
pub fn line(status: &Status<'_>) -> RenderLine
```
- Shape when wide: `<model> · <activity word> · <n> turns · <hint>` — the hint is what the loop
  offers the user; make it a field (`hint: Option<&'a str>`), `None` renders no separator for it.
- Activity words: `Waiting` → `idle`, `Running` → `working`.
- `turns == 0` renders no turn segment at all: "0 turns" says nothing worth spending a column on.
- Roles: model → `Meta`; activity → `Status`; turn count → `Meta`; hint → `Muted`; the `·`
  separators → `Muted`.
- Narrow behaviour, in this order, dropping whole segments rather than wrapping: drop the hint,
  then the turn count, then truncate the model with `width::truncate`. **Model and activity always
  survive**, because a status line that does not say which model is answering has stopped being one.
  Below `MIN_COLUMN` still emit something (the activity word alone is honest); never an empty line.
- `Running` is the only state a busy session can be in while the loop draws. A cancelled or failed
  turn is reported by the loop itself, not by this line — say so in the doc comment so nobody reads
  this module as the turn-status surface.

## Constraints
- `pi-rs-tui` imports nothing from runtime/store/core-provider; it may depend on `pi-rs-core` only.
- Pure and deterministic: same snapshot, same line. No clock, no env, no IO.
- New file `crates/pi-rs-tui/src/statusline.rs`, `pub mod statusline;` plus re-exports at the crate
  root, matching how `editor`/`keys` are exported in `lib.rs`.
- 2-space indent, 100 columns, edition 2024.

## Tests expected (in the module, no terminal)
wide renders all segments in order · `turns: 0` renders no turn segment · `hint: None` renders no
dangling separator · each drop rule in turn · model+activity survive the narrowest width · roles as
assigned (assert via the segments, not via ANSI) · `plain()` of the narrowest case is non-empty.

## Verify (paste real numbers)
`cargo fmt --all --check` · `cargo clippy --workspace --all-targets -- -D warnings` ·
`cargo test -p pi-rs-tui` and `cargo test --workspace` · `bash bench/startup.sh`

Commit `--allow-empty -m "chore: start"` first, then commit after every step, always compiling.
Do not merge, rebase, push, open PRs, or touch other branches.
