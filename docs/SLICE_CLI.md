# Slice: help must not advertise a flag the parser rejects

Verified by grep (do not re-derive, but do re-check before use): `src/cli.rs` — `pub fn parse(...)` at
line 143, `parse_run` line 167, `parse_trace` line 262; unknown-input errors are the strings
`unknown command '{...}'` (line 161), `unknown run argument '{flag}'` (line 235), `unknown trace argument
'{other}'` (line 350). Help text lives in consts (`TOP_HELP`, used at line 161; `RUN_HELP`, used at 235;
trace help likewise) as string literals containing lines like `"  --help                   Show this help.\n"`.

## What to pin

A test that derives the documented flag set **from the help strings themselves** — scan each help const for
tokens matching `--<lowercase-letters(-letters)*>` — and asserts the parser recognises every one of them:

* call `parse()` with the command the help belongs to plus the documented flag (supply a placeholder value
  where the flag takes one, e.g. `--config x`, and use the bare flag where it does not);
* the assertion is that the result is **never** an error containing `unknown`. A `expected a value`-style
  error is acceptable and must be distinguished from `unknown` — say so in the test so a future reader
  knows why the assertion is on the word `unknown`, not on `is_ok()`.

Plus: the token `--help` is accepted on every command whose help mentions it, and an undocumented
nonsense flag (`--definitelynotreal`) still produces an `unknown` error — otherwise the test above could
pass because the parser stopped reporting `unknown` for anything.

**If a documented flag is rejected, that is the finding.** Keep the failing assertion, name the test after
what was found, and report the exact error string. Do not edit help text or the parser to make it green;
the finding is worth more than a green suite here.

## Evidence rule

Every path, line, const name, and error string you report must be pasted from command output in the same
message. Empty grep means **`not found`**. If the consts or functions differ from the above, grep wins:
document the mismatch and write the test against real code. Never invent a help const.

Tests-only, in `src/cli.rs`'s existing `mod tests`. ≤3 `read` calls at ≤40 lines; outputs ≤20 lines;
`cargo test --bin pi-rs` while iterating, `--workspace` once at the end; commit before running cargo.
No ROADMAP tick. No merges, rebases, pushes, PRs; stay in this worktree; `git rev-parse HEAD` before each
commit — stop and report if it moved without your commit.
