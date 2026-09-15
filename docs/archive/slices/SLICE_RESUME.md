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

---

# Step 2: retire the refusal with a real continuation

Step 1 (`2edffec`) resolves the name and **refuses** to append, because the request is built from the
prompt alone. Keep every step-1 test passing except the ones that assert the refusal itself, and update
those to assert continuation instead.

1. Resolve the id through the **same** `resolve_session` the trace command uses (exact id or unique
   prefix; ambiguous = error). No second resolver.
2. Rebuild the model-visible context from what the store actually holds — checkpoint plus
   post-checkpoint active events, per `AGENTS.md`. Do **not** hydrate the full session when a
   checkpoint path exists.
3. Append the new turn to the **same session id**, through the existing store append path. No new
   session, no second file.
4. If the rebuild is impossible, keep failing loudly with the missing part named; a partial context
   must never be silently presented as a continuation.

Required test additions (same file, provider-free where possible): a recorded session gains a second
turn under the **same** id and the store holds exactly one session; the second request's model-visible
messages contain the first turn's prompt (assert what the provider is actually sent).

Step-1 tests that must stay: leading-dash rejection, missing value, unknown id → non-zero exit and no
store/session created, ambiguous prefix → error.
