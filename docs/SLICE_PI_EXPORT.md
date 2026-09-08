# Slice: export a pi-rs session in Pi's shape (round-trip proven, not assumed)

`src/import_pi.rs` reads Pi session JSONL into pi-rs; the fixtures in
`crates/pi-rs-store/tests/fixtures/pi/` are the ground truth for what Pi actually writes. There is no
way out. Add it, and prove it by **round-tripping through the existing importer** rather than by
eyeballing the spec.

## Contract

1. `pi-rs export <session-id> [--out <path>]` writes that session as Pi-shaped JSONL (stdout when
   `--out` is omitted). Resolve the id with `crate::trace::resolve_session` — exact id or unique prefix,
   ambiguous = error — the same resolver `run --resume` and `trace` use. No second resolver.
2. The emitted shape is derived from what `import_pi` **accepts**, field for field, using the fixtures
   as the reference. Do not invent fields, timestamps, or ids the importer ignores.
3. Anything the canonical trace holds that Pi's shape cannot carry — reasoning **provenance** is the
   obvious candidate — must be named on stderr as dropped. Silent loss of provenance is exactly what
   this project forbids.
4. Unknown id → non-zero exit, nothing written. Leading-dash values rejected like the other flags.
5. `--out` must not escape the store directory by accident: create parent dirs only under the given
   path, no symlink-following cleverness.

## Required tests (integration, drive the binary)

* export → `import_pi` into a fresh store → the re-imported session's user and assistant content
  matches the original **in order** (assert on content and ordering, not on byte equality of the file);
* a session whose trace carries a provider reasoning summary emits a stderr "dropped:" line naming it,
  and still exports successfully;
* unknown id → non-zero, no file created; ambiguous prefix → error naming the candidates;
* leading-dash id → rejected before anything is opened.

## Context discipline

One `grep -n` per file with `| head -12`; at most **five** `read` calls, each ≤40 lines with
`offset`/`limit`; command output ≤15 lines; `cargo test --workspace` at most once total, use
`cargo test --test <name>` in between; **commit after every green step**. Failing at the cap with a
committed green step is a pass. Never weaken an assertion to get green; a reported gap with `file:line`
beats a fake test. Leave ROADMAP boxes unchecked. No merges, rebases, pushes, PRs; stay in this
worktree. `git rev-parse HEAD` before each commit — if it moved without your commit, stop and report.
