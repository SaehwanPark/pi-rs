## Problem

What is wrong, missing, or unmeasured today — and why it matters now. Name the command,
session file, or benchmark that shows it. If this closes a roadmap line, quote it.

## Change

What this pull request does, in the order a reviewer should read it. State the decisions
that were actually choices, with the reason: an alternative a reviewer can guess at is an
alternative a reviewer will ask you to revisit.

## Testing

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] `cargo doc --workspace --no-deps`
- [ ] compatibility fixture added or updated (`pi-rs-compat`), when Pi behaviour changed
- [ ] benchmark run or added, when a startup, render, resume, context, package, MCP, or
      extension-host path changed

Which tests cover the new behaviour, and which failure paths they prove. Paste counts, not
"tests pass".

## Not changed

The surfaces this pull request deliberately leaves alone, so a reviewer does not have to
diff them to find out.

## Merge order

Independent of the open stack, or: merge after #NNN (base `branch`). Roadmap line ticked:
yes / no / doc-only conflict expected.
