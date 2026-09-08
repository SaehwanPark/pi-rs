# Slice: pin the provenance vocabulary

Verified by grep, do not re-derive: `ReasoningProvenance` is `crates/pi-rs-core/src/provenance.rs:25`,
`#[serde(rename_all = "snake_case")]` line 24, four variants, `as_str()` line 38, `label()` line 51,
`is_inferred()` line 64.

`AGENTS.md` rules 4 and 5 forbid conflating the four kinds of reasoning, and rule 5 forbids claiming
hidden chain-of-thought was recovered. **Nothing today stops a rename from quietly merging two labels.**
Pin the vocabulary in tests.

* golden test over **all four variants**: `as_str()` and `label()` each equal the exact literal currently in
  source (`native`/`reasoning`, `provider_summary`/`provider summary`, `declared`/`declared rationale`,
  `reconstructed`/`reconstructed rationale`).
* serde stability: each variant serialises to its snake_case tag and deserialises back; the JSON string is
  asserted literally, not via a round-trip alone (a symmetric rename would otherwise stay invisible).
* distinctness: the four `as_str()` values are pairwise distinct, the four `label()` values are pairwise
  distinct, **and** no `label()` equals any `as_str()` of a different variant. This is the rule the doc
  comment on `label()` states — the two vocabularies must not be interchangeable.
* `is_inferred()` is true for exactly `Declared` and `Reconstructed`, false for the other two.

Evidence rule: paste the command output behind every claim; empty grep means **`not found`**; if the file,
line, or literal differs from the above, grep wins — document the mismatch and write the test against the
real source. If a golden assertion fails because the current literals differ from the list above, report
both values; do not "fix" source.

Test-only. Prefer `crates/pi-rs-core/tests/` (new file) unless the crate's convention puts these in-unit —
check what exists first with `ls crates/pi-rs-core/tests/ | head` and say which you chose and why.
≤3 `read` calls at ≤40 lines; outputs ≤20 lines; `cargo test -p pi-rs-core` only, `--workspace` once at the
end. No ROADMAP tick. No merges, rebases, pushes, PRs; stay in this worktree; `git rev-parse HEAD` before
each commit — stop and report if it moved without your commit.
