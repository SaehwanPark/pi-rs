# Slice: classify an input line into operation vs arguments (pure, unwired)

ROADMAP: "Implement syntax highlighting for operation vs arguments". This slice is the **classifier
only**. Wiring it into the editor/live surface is a later slice, and the status-line pair
(`statusline` then its wiring) is the precedent: land the pure projection first, prove it, then wire it.

## What already exists — use it, do not redefine it

`crates/pi-rs-tui/src/style.rs:31` `Role::Operation`, `:37` `Role::Argument`;
`crates/pi-rs-tui/src/line.rs:21` `pub struct Segment { pub text: String, pub role: Role }`. Both are
re-exported at the crate root. Emit those; add no new role unless a case genuinely needs one (if it
does, say which and why in the doc comment).

## Contract

`pub fn tokens(input: &str) -> Vec<Segment>` in a new `highlight` module, re-exported at the crate root
as `highlight::tokens` — **not** as a bare `tokens` beside whatever else the prelude carries.

1. The first non-whitespace run is the **operation**; every following run is an **argument**.
2. Runs are separated by whitespace. A quoted run — `'…'` or `"…"` — is **one** segment even if it
   contains spaces, and the quote characters stay in the segment text (this is a classifier for colour,
   not a shell parser: it must never claim to reproduce argv).
3. An **unterminated** quote is not an error: the run extends to end of input and stays an argument.
4. Backslashes are literal. No escape processing, no expansion, no `$(…)`/`$VAR` interpretation. Say so
   in the doc comment.
5. Whitespace-only or empty input → empty `Vec`, documented, not a panic and not a phantom segment.
6. Concatenating every segment's `text` in order, **plus** the separators, must reproduce the input
   exactly, including the original spacing. Add a property-ish test over a table of inputs that asserts
   this by re-joining with the preserved separator segments... if you preserve separators as segments
   they need a role; prefer the simpler rule: separators are **not** emitted, and the test asserts
   segment texts equal the expected token list. State which rule you took in the doc comment.
7. Pure: no I/O, no allocation beyond the output, no Unicode width assumptions here (width is the
   caller's problem).

## Tests (unit, in-module)

Operation-only input · operation + two arguments · double-quoted argument containing a space ·
single-quoted likewise · adjacent quotes `"a"b` stay one argument · unterminated quote · leading
whitespace ignored · empty and whitespace-only inputs · a non-ASCII argument is not split on
byte-boundary characters.

## Limits

Depend on nothing new (no new crates in `pi-rs-tui`). At most **three** `read` calls, ≤40 lines each;
one `grep -n` per file with `| head -12`; command output ≤15 lines; `cargo test --workspace` once at the
end, `cargo test -p pi-rs-tui` in between. **Commit before running any cargo command.** Never weaken an
assertion. Do not wire it into the editor or live surface. Leave ROADMAP unchecked.
No merges, rebases, pushes, PRs; stay in this worktree. `git rev-parse HEAD` before each commit — if it
moved without your commit, stop and report.
