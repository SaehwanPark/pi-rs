# Phase 7 change brief — `rkb-rs` first-party integration

## Scope

Implement the smallest complete Phase 7 vertical slice:

1. Add generic, typed `ExternalContextRef` semantics without making `rupi-core`
   depend on `rkb-rs`.
2. Preserve retrieved external context in the durable session projection and
   canonical trace, including source/citation metadata; provide an explicit
   inline-to-reference transformation and a resolver boundary for rehydration.
3. Add a standalone `rupi-rkb` integration crate that understands the verified
   `rkb-rs` MCP contract, provides setup/discovery and a bundled RKB skill,
   lazily activates the existing MCP client, parses citation-bearing
   `get_agent_context` results, creates external-context items, and rehydrates
   references by durable record id.
4. Add focused fixtures/tests and update only evidence-backed roadmap and design
   documentation.

## Verified upstream contract

The inspected `/tmp/rkb-rs` checkout (GitHub `SaehwanPark/rkb-rs`, default branch
`main`, commit `95c0ca89683b4b366963e9eb90d838b659dd70f6`) exposes a read-only
stdio MCP server via `rkb mcp`. Its tools are:

- `search_datasets`, `search_documents`, `search_variables`, `search_chunks`;
- `get_agent_context` with `{query, limit}` arguments.

`get_agent_context` returns JSON text shaped as `{query, result_count, entries}`;
each entry has `citation`, `record_id`, `record_type`, `title`, `dataset_id`,
`score`, `snippet`, `source_url`, `source_document`, and optional `page`.
`rkb mcp` is configured as a stdio server with command `rkb` and argument `mcp`.
The RKB project is public-document-only and citation-backed; no direct crate
linkage is required or permitted for this integration.

## Boundary and invariants

- `rupi-core` remains provider/MCP/RKB independent; `rupi-rkb` is downstream.
- RKB setup and discovery are pure/config-driven and perform no process, network,
  filesystem, or index work during normal startup.
- MCP activation remains explicit/lazy; no RKB server is connected merely because
  it is configured or discovered.
- A reference is never inferred from free-form text. Provider, durable resource id,
  citation, provenance, and RKB source metadata remain typed and serializable.
- External-context retrieval is distinct from reasoning and never implies hidden
  chain-of-thought. Missing/invalid resources fail closed.
- Canonical trace remains authoritative; context compaction may replace the model
  visible form but never deletes the source event.
- Persisted evidence passes through the existing store redaction boundary.

## Acceptance evidence

- Core contract tests cover reference serialization, inline-to-reference
  compaction, metadata preservation, and retrieval-event round trips.
- Store/runtime tests prove external context is session-persisted and resumes with
  its reference/source metadata.
- `rupi-rkb` tests cover verified MCP JSON parsing, citation rendering, setup and
  discovery, lazy activation, exact resource-id rehydration, and unavailable
  resource failure.
- A gate fixture demonstrates retrieve -> inline context -> compacted reference
  -> on-demand rehydration with unchanged provenance/source metadata.
- Workspace checks and the relevant startup/restore checks pass; no `rkb-rs`
  dependency appears in `rupi-core`.

## Non-goals

No direct dependency on the external `rkb-rs` crate, no eager indexing or network
access, no write-capable RKB tools, no generic MCP resource protocol expansion, and
no unrelated Phase 8+ host/worker/replay/optimization work.
