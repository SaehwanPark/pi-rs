# Slice: resume one durable session at launch

`pi-rs` holds one session across many turns **inside a process** and `pi-rs trace <id>` reads a session
back out. There is no way to **continue** a stored session in a new process. Add it.

## Observable contract

1. `pi-rs --resume <session-id>` (with `--store/--config/--cwd` honoured as usual) continues that
   session: the next turn is appended to the **same session id**, not a fresh one.
2. A value starting with `-` is rejected like `--store`/`--config` already reject theirs.
3. Unknown or unreadable id → clear error on stderr, non-zero exit, and **no new session created**.
4. Nothing at parse time opens the network or scans the store.
5. Do not fabricate a continuation. If the runtime cannot rebuild the model-visible context from what
   the store actually holds, fail loudly and say which part is missing.

## Required tests (integration, drive the binary; copy the fixtures style already in `tests/`)

* resume appends to the same session id and the store still holds exactly that one session;
* the resumed turn's model-visible context contains the earlier turn's prompt (assert on what the
  provider is actually sent, or on the canonical trace if the deterministic provider path exposes it);
* unknown id → non-zero exit, no session written;
* leading-dash value → rejected before any file is opened.

## Context discipline (this kills agents; obey it)

One file per `grep -n`, each `| head -12`; `read` at most 5 times with `offset`/`limit` ≤ 40 lines;
command output ≤ 15 lines; `cargo test --workspace` at most twice per step (`--test <name>` otherwise).
**Commit after every green step.** Failing at the cap with a committed green step is a pass.
Report ≤ 25 lines. Never weaken an assertion to get green. Do not mark any ROADMAP gate checked.
No merges, rebases, pushes, PRs; do not touch other worktrees. Check `git rev-parse HEAD` before each
commit; if it moved without your commit, stop and report.
