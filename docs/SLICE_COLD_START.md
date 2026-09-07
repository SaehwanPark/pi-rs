# Slice: a cold-start benchmark that reports something true

ROADMAP line "Add cold-start benchmark" is still open; `bench/startup.sh` measures **warm** runs of
the same binary path and says so. This slice adds the cold half — and is allowed to conclude that a
given machine cannot measure cold start honestly, as long as it proves that rather than inventing a
number.

## Definition, pinned (write it in the script header)
Dropping the page cache needs root, which this project does not assume. So define cold as:
**the first execution of a freshly-linked binary inode, with no prior exec of that inode** — copy
`target/release/pi-rs` to a unique path per iteration and time the first exec of that path against
the second and third execs of the same path. That isolates first-exec effects (page-in of the
text, relocation, dynamic linking) from steady-state, which is the thing a user on a cold machine
actually pays more of.

## Deliverable: `bench/cold_start.sh`
- Build release once (`cargo build --release`, pipe to `tail -3`).
- For N iterations (default 5): fresh copy to a unique path; time exec #1 (cold), then execs #2/#3
  (warm), using the same trivial invocation `bench/startup.sh` uses — check what that script runs
  before choosing, and use the same one.
- Report min / median / max for cold and warm, and the delta. Same output shape as `bench/startup.sh`
  so existing tooling and habits carry over.
- **Do not write a budget you have not measured.** If you do write one, use the rule from
  `crates/pi-rs-tui/benches/keystroke.rs` (median x 10, floored) and say why that floor.
- Header states plainly: this is a pre-merge gate on a dev machine, **not** a CI check, because CI
  runners have page-cache behaviour that makes the number meaningless.

## If the delta is noise
Say so, with the numbers, and record what a real cold-start measurement would need (root cache drop,
`posix_fadvise`/reclaim control, or a VM boot harness). An honest "not measurable here, here is the
proxy we do have" is a pass for this slice; a fabricated budget is a fail.

## Constraints
- Shell + docs only. No Rust changes, no new dependency, `bench/startup.sh` semantics unchanged.
- `bash -n bench/cold_start.sh` clean; make it executable.
- ROADMAP: tick the cold-start box **only** if you consider the delivered script an honest cold-start
  benchmark; otherwise annotate it the way the keystroke item is annotated.

## Verify (paste real numbers)
`bash -n bench/cold_start.sh` · run `bash bench/cold_start.sh` twice and paste both (a number that
moves wildly between runs is a finding, not a rounding detail) · `bash bench/startup.sh` unchanged ·
`cargo test --workspace` still 492 passed · `cargo clippy --workspace --all-targets -- -D warnings`
clean.

Commit `--allow-empty -m "chore: start"` first, commit after every step. Keep command output under
20 lines. Do not merge, rebase, push, open PRs, or touch other branches or worktrees.
