# Slice: warm-start benchmark

`bench/startup.sh` measures a bare launch; `bench/cold_start.sh` measures first-exec-of-a-new-inode.
Neither measures the thing #32 made possible: **relaunching a process that continues a stored session**.

## Definition to pin in the header

Warm start = time from exec to exit for `pi-rs run --resume <id> --prompt <p>` against a session that
**already exists in the store**, compared against the same argv **without** `--resume` on an empty
store. It measures resolve + `Store::restore` + context rebuild. It is *not* a cold-start number (the
page cache is warm, the binary inode is warm) and the header must say so.

## Script contract

* `bench/warm_start.sh`, mode 755, `bash -n` clean, no root, no network beyond whatever the configured
  provider needs.
* Fixture: create the session by running the binary once into a temp store using the same config/env a
  user would. If that first run cannot complete (no provider configured, endpoint unreachable), print
  `SKIP: <one-line reason>` and exit **0** — this is a pre-merge gate on a dev machine, not a CI check,
  and a red CI because someone's laptop has no endpoint is a false alarm.
* Same printed shape and flags as `bench/startup.sh` (`--iterations`, `--json`, `--help`), min/mean/
  median/max for both arms plus the median delta.
* Clean up the temp store on exit (`trap`).
* **Do not invent a budget.** `cold_start.sh` shows why: its own numbers would not hold a sign. If the
  delta is inside the noise the printout says so.

## Verification

`bash -n` clean · mode 755 · script run recorded, output included in the PR · `cargo test --workspace`
unchanged (shell only) · `bash bench/startup.sh` still works. Leave every ROADMAP box unchecked;
annotate only if the annotation states what was *not* measured.

Context discipline: one `grep -n` per file with `| head -12`; `read` at most 3 times, ≤40 lines each;
command output ≤15 lines; commit after every green step; report ≤20 lines. Check `git rev-parse HEAD`
before each commit; if it moved without your commit, stop and report. No merges, rebases, pushes, PRs;
stay in this worktree.
