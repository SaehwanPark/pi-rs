# Task: produce a tested, conflict-free merge of #16 onto #12 (side branch only)

Base is `feat/prompt-templates` (PR #12). Merge `origin/feat/pi-import` (PR #16) into it. The conflict set
was measured by an actual merge: **2 hunks in `src/main.rs`, 4 in `src/cli.rs`**. `COMPATIBILITY.md` and
`ROADMAP.md` auto-merge.

Both sides add an unrelated subcommand. **Every hunk is take-both** — no semantic decision is involved:

* `src/main.rs` — keep `mod prompts;` **and** `mod import_pi;`; in the `cli::Command::*` dispatch match,
  keep the `Prompts`/`Prompt` arms **and** append `Import`'s arm.
* `src/cli.rs` — keep both `Command` enum variants, both parse arms, both blocks of help text, and both
  sides' tests.

If any hunk is **not** of that shape — e.g. two sides chose the same variant name, one side deleted what
the other extended, or a test exists on one side only under a different name — **stop** and report the hunk
verbatim. Do not invent a resolution for something that is not take-both.

## Then prove the merge, don't assert it

* `cargo build` must succeed; `cargo test --workspace` must pass **and** the count must be **at least
  `max(count on #12 alone, count on #16 alone)`**. Record all three numbers. If the merged count is lower
  than either side, a test was silently dropped in resolution — that is the failure mode of take-both, and
  it must be reported, not smoothed over.
* `cargo fmt --all --check` clean; `cargo clippy --workspace --all-targets -- -D warnings` clean (CI parity).
* Both subcommands must actually run: `cargo run -q -- prompts list` style invocation for the #12 command
  and the import command from #16 — use each PR's own help text for exact syntax; report real output or the
  real error, ≤20 lines each.

## Hard limits

Commit the merge, **then** run cargo (never before). **Push only to `feat/pi-import-rebased-on-12`** —
never force-push `feat/pi-import`, never touch PR #16, #12, or `main`, never open a PR, never rebase.
Stay in this worktree. Evidence rule: paste command output behind every claim; empty grep means
`not found`; ≤3 `read` calls at ≤40 lines.
