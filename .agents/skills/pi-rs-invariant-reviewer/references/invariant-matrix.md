# pi-rs Invariant Review Matrix

Use changed-surface routing; do not mechanically require every check.

| Changed surface | Highest-risk questions |
| --- | --- |
| Agent loop | Is one model active? Are effects committed before continuation? Is workflow policy absent? |
| Provider | Are capabilities and failures normalized? Are semantic failures excluded from failover? |
| Events | Are identity, ordering, persistence, replay, and UI relevance explicit? |
| Session/trace | Is canonical history separate from working context? Can resume avoid deep hydration? |
| Reasoning | Is provenance preserved through serialization and rendering? Is hidden reasoning never implied? |
| Tools | Are lifecycle and mutation properties explicit? Is `Unknown` preserved and never blindly replayed? |
| Context | Does compaction preserve trace, constraints, unresolved work, and artifact references? |
| Failover | Do retries precede takeover? Are capabilities checked? Does continuation use committed state? |
| MCP | Is protocol detail behind an adapter? Are connection and schema exposure lazy? |
| Pi compatibility | Is the claim versioned, fixture-backed, per-surface, and isolated from core? |
| TUI | Does it consume semantic events without owning runtime state? Are rare events clear and routine output quiet? |
| Startup | Could this trigger network, Node, MCP, deep hydration, full scans, or heavy allocation before readiness? |
| Security/trust | Does redaction precede persistence? Are raw payloads opt-in? Is project-local power trust-gated? |

## Cross-Cutting Checks

### Correctness and explicit state

- No important state is encoded only by absent values, booleans, or ad hoc strings.
- Unknown information remains unknown rather than being coerced.
- Error handling preserves the distinction between retry, failover, refusal, cancellation, and semantic failure.
- Failure paths have tests, not only comments.

### Canonical record and replay

- Model-visible context is derived and replaceable.
- Event ordering is deterministic.
- Large payload storage does not destroy references needed for replay.
- Historical events cannot be mistaken for newly generated continuation.

### Provenance

- Native reasoning, provider summaries, declared rationales, and reconstructed rationales cannot serialize or render identically.
- Model epochs attribute output and artifacts to the producing model.
- External evidence retains source identity through compaction and rehydration.

### Side effects and recovery

- Every tool call has durable identity and lifecycle.
- Mutating operations are not replayed when completion is uncertain.
- Mid-turn failover sees committed reads, edits, and test results.
- Backup incompatibility is reported or explicitly degraded, never hidden.

### Performance and dependencies

- Optional systems remain lazy.
- Startup work answers the question: must this happen before first input or submission?
- Hot paths avoid unnecessary allocation, cloning, directory scanning, and concurrency.
- A new dependency has concrete value and acceptable binary/startup cost.

### Scope and compatibility

- Core additions improve fundamental reliability, inspectability, compatibility, context efficiency, or composability.
- Domain knowledge and user workflow orchestration stay outside core.
- Compatibility changes include fixtures and honest status vocabulary.
- Undocumented Pi internals are not implemented merely to claim parity.

### Documentation and roadmap

- New semantic events document ordering, persistence, replay, and UI impact.
- Architecture docs change only when the contract changes.
- `ROADMAP.md` reflects started, completed, blocked, or altered stage-gate work.
