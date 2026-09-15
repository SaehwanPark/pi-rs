# Phase 9 — MCP server / worker mode

## Scope

Implement the bounded MCP worker boundary in `pi-rs-mcp` without coupling the MCP
protocol to providers, terminal rendering, or runtime implementation details.

## Contract

- `agent.start`, `agent.continue`, `agent.cancel`, `agent.branch`, and `agent.compact`
  return typed coarse results.
- A `WorkerEngine` adapter is the explicit boundary for headless `TurnLoop` composition;
  the service owns asynchronous run handles, per-run cancellation, bounded `wait_ms`, and
  state transitions.
- Resources are `session://<id>/state`, `summary`, `messages`, `trace`, `diff`,
  `artifacts`, and `checkpoint/latest`.
- Summary is external authored/provider/runtime text with explicit provenance. Trace is a
  bounded projection of ordering/attribution/provenance and never raw event payloads or
  inferred hidden reasoning.
- Diff/artifact/checkpoint absence is explicit; current-state branching is supported while
  checkpoint/event branches are rejected until an engine supplies historical snapshots (Phase 10).
- `serve_stdio` handles MCP initialize, tools/resources listing and calls, notifications,
  malformed JSON-RPC, and bounded request lines without terminal scraping.

## Verification evidence

- `cargo test -p pi-rs-mcp --all-features`: worker unit tests plus existing MCP tests pass.
- `crates/pi-rs-mcp/tests/worker_integration.rs`: public worker run/cancel/resource/MCP tests.
- Workspace tests, clippy, docs, startup, and render benchmarks pass.
- Invariant review checks cancellation, summary/trace separation, epoch/failover history,
  resource URI safety, bounded projections, and no implementation/path exposure.
