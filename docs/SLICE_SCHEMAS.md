# Slice: document the event, session, and provenance schemas

ROADMAP Phase 2 item: *"Event, session, and provenance schemas are documented."* Document what the code
actually defines — no invented fields, no aspirational schema.

Write `docs/SCHEMA_REFERENCE.md`, three sections:

1. **Events** — enumerate `AgentEvent` variants from `crates/pi-rs-core/src/event.rs`: name, payload
   fields with types, and one line of purpose. Include the sequencing contract (`EventSeq`, session id,
   timestamps) as the code expresses it.
2. **Session records** — how a trace persists: the JSONL shape written by the store, the role of the
   checkpoint/barrier, and `session_record_kind()`'s mapping in `src/session.rs` (field names must be
   quoted from that code).
3. **Provenance** — `ReasoningProvenance` variants and the rule that rendering must preserve the
   distinction (Native / ProviderSummary / Declared / Reconstructed). Quote `AGENTS.md` on this rather
   than paraphrasing.

**Accuracy is the deliverable.** For every variant, check whether production code actually constructs it.
Add a **Producers** line: `yes (file:line)` or **`none found`**. Issue #38 already found three compaction
variants with zero producers — do not silently repeat that mistake, and do not fix it either.

Also state explicitly what is **not** in scope: no schema changes, no new event variants, no serde
attribute changes, no ROADMAP tick (a human confirms the doc is complete first).

## Limits

Read with `offset`/`limit`, ≤40 lines; enumerate variants with `grep -n` piped through `| head -40`; keep
every command's output under ~25 lines. Never `cat` `event.rs`. Doc-only — no code changes. Commit after
writing the Events section, then after each subsequent section (three commits). `cargo doc` is not
required; `cargo fmt --all --check` must stay clean. `git rev-parse HEAD` before each commit — if it moved
without your commit, stop and report.
