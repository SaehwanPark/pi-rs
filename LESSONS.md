# LESSONS

Durable, verified lessons for contributors to this repository. Keep entries small and
evidence-backed; delete one when its prevention becomes structurally enforced.

## Merging a stacked PR series out of band leaves every PR "open"

- Context: 36 CI-green PRs, each stacked on another branch, all landed into `main`
  with local `git merge --no-ff` commits pushed in one go.
- Symptom: `gh pr list --state open` still shows all 36 after the push, and
  `gh pr diff N` keeps reporting a file diff, even though every head commit is in
  `main`.
- Cause: GitHub marks a PR merged only when the merge lands through its own merge
  path (or a matching merge commit lands per-push); a batch push of pre-made merge
  commits does not close them. `gh pr diff` is computed base...head, not "is the
  head reachable from base".
- Resolution: verify containment with
  `git merge-base --is-ancestor "$(gh pr view N --json headRefOid -q .headRefOid)" main`,
  then `gh pr close N --comment "superseded by integration merge <sha>"`.
- Prevention: for a batch integration, script the ancestor check + close as part of
  the merge loop, right after pushing `main`.

## Resolve a many-branch merge in topological order, and take "both" for append-only lists

- Context: the same files (`ROADMAP.md` checkbox lists, `src/cli.rs` help tables and
  flag lists, `src/main.rs` dispatch arms) were edited by nearly every branch.
- Symptom: the same 3-way conflict shape repeated on every merge; a wrong
  "ours/theirs" choice silently deleted another branch's line (a lost `interactive`
  help row failed `tests/interactive_cli.rs` late).
- Cause: these files are append-only lists; whole-file ours/theirs choices drop the
  other side's entries.
- Resolution: merge leaves-first in stack order; for these files resolve hunk-wise
  taking both sides (prefer `[x]` on checkbox state, keep the evolved help format,
  union flag lists and dispatch arms), then run `cargo check --workspace
  --all-targets` after every few merges instead of only at the end.
- Prevention: when a conflict is "two lines added at the same slot", the answer is a
  union, not a choice; the compile after each merge catches signature evolution
  (e.g. `open_session` gaining a `resume` parameter, `after_turn` returning
  `TurnReport`) while the cause is still obvious.

## Never start a mass merge with uncommitted plan documents in the tree

- Context: `ROADMAP.md` carried an uncommitted "next best steps" section when a
  36-PR integration began.
- Symptom: mid-merge `git checkout --theirs ROADMAP.md` (the standard resolution for
  a parallel rewrite of that file) silently discarded the uncommitted section; it
  existed in no commit, no branch, and was unrecoverable.
- Cause: checkout-based conflict resolution overwrites the worktree copy; anything
  uncommitted in that file is gone, and merges do not warn about it.
- Resolution: reconstructed the section from the merged state and marked it as
  reconstructed in the file itself.
- Prevention: commit or stash plan/status docs before the first merge commit of any
  batch; `git status` must be clean for files a merge is expected to touch.

## Help-surface compatibility tests pin the exact top-level help shape

- Context: the top-level help moved from `pi-rs <cmd> [options]` usage lines to a
  `Commands:` table; a test asserted `contains("pi-rs interactive")`.
- Symptom: an unrelated-looking integration failure in
  `tests/interactive_cli.rs::top_level_help_names_the_interactive_command` after a
  help-table merge resolution.
- Cause: the test pinned the old prefixed shape; help-text merges are compatibility
  changes, not cosmetic ones.
- Resolution: keep the table canonical, update the assertion to the bare command
  name with a comment saying why, and keep every command row present (the lost
  `interactive` row was itself a regression).
- Prevention: when resolving conflicts in `TOP_HELP`/`RUN_HELP`, diff the rendered
  `pi-rs --help` output against every `tests/*_cli.rs` assertion before committing
  the merge.
