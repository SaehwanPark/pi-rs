# Session Compatibility and Non-Round-Trippable Metadata

`pi-rs` provides bidirectional session interoperability with Pi through `pi-rs import-pi`
and `pi-rs export`. This document records the architectural boundary between the two
session representations, the field mappings in each direction, and the full catalog of
non-round-trippable metadata.

---

## 1. Design Principles

1. **Stronger Internal Model**: `pi-rs` maintains a rigorous internal event log
   (`sessions/<id>.trace.jsonl`) and semantic session journal (`sessions/<id>.jsonl`). We never weaken internal provenance,
   typed tool execution states, or crash-safety invariants to match foreign serialization quirks.
2. **Replay Safety**: An import never executes anything. A foreign tool result records work
   already performed; `pi-rs` files it as historical record without re-execution.
3. **Honest Provenance**: An import never claims foreign material is native. Foreign thinking
   blocks receive no provenance rather than a fake `Native` claim.
4. **Explicit Loss Reporting**: Export and import name every dropped, adapted, or unrepresentable
   metadata element on stdout/stderr. No metadata is dropped silently.

---

## 2. Structural Models Compared

| Aspect | Pi Session JSONL | `pi-rs` Session Store |
|---|---|---|
| **Storage Shape** | Single flat `.jsonl` file | Semantic `sessions/<id>.jsonl` + canonical `sessions/<id>.trace.jsonl` + WAL, checkpoints, and per-session blobs |
| **History Topology** | Directed entry tree (`id` / `parentId` links) | Linear conversation turn sequence + append-only trace log |
| **Tool Execution** | Content blocks in assistant messages + `toolResult` messages | 5-state lifecycle: `Requested`, `Started`, `Completed`, `Failed`, `Unknown` |
| **Reasoning** | Untyped `thinking` blocks | 4-tier typed provenance: `Native`, `ProviderSummary`, `Declared`, `Reconstructed` |
| **Large Payloads** | Stored inline or truncated with notice | Content-addressed blob store (`blobs/sha256/...`) above inline threshold (8 KiB) |
| **Model Attribution** | Per-entry optional `provider`/`model` | Model epochs (`ModelEpochStarted`), explicit failovers, per-envelope attribution |
| **Safety & Privacy** | Stored as emitted | Durable redaction tracking (`redactions` count per line, secret masking) |

---

## 3. Pi -> `pi-rs` (`pi-rs import-pi`)

`pi-rs import-pi <path>` imports a Pi `.jsonl` file or directory of session files into the `pi-rs`
store.

### 3.1 Mapped Fields

- **Header**: Version 1, 2, or 3 headers map to `SessionHeader` with `id: "pi-<id>"`, `cwd`,
  and starting timestamp.
- **User Messages**: Role `user` entries map to `AgentEvent::UserMessage` and session message turns.
- **Assistant Messages**: Deliberately folded into a request span: `ReasoningDelta`,
  `AssistantDelta`, `ToolRequested`, and `ModelRequestCompleted` (carrying usage token counts).
- **Tool Results**: Mapped to `AgentEvent::ToolCompleted` and corresponding message turns.
  Payloads exceeding the store's inline threshold (8 KiB) are moved to the session blob store.
- **Model / Thinking Changes**: Mapped to informational `Diagnostic` events.

### 3.2 Dropped or Adapted Metadata on Import

| Foreign Field / Entry | Handling in `pi-rs` | Loss Report / Diagnostic |
|---|---|---|
| **Entry Tree Branches (`off_path`)** | Only the active path from newest leaf to root is imported. Fork branches are excluded. | Stderr report: `"{count} entries were not on the path from the newest entry written"`. |
| **Cross-file Lineage (`parent_session`)** | Each file becomes an independent session. Parent lineage across files is not linked. | Stderr report notes parent session id without building cross-file DAG. |
| **`compaction` Entries** | Boundary marked as a diagnostic. Compaction summary text is discarded to prevent duplicate conversation replay. | Stderr report names compaction event count. |
| **Extension Entries (`label`, `custom`, `custom_message`)** | Skipped; `pi-rs` core models coding agent lifecycle events, not UI or third-party extension states. | Stderr report names each unhandled entry type and count. |
| **Provider Cost & Cache Usage** (`usage.cacheRead`, `usage.cacheWrite`, `cost`) | Dropped. `pi-rs` tracks exact input/output tokens in `ModelRequestCompleted`, but excludes billing metadata. | Omitted from stored event envelope without error. |
| **Unattached Image Entries** | Images lacking inline bytes are counted as attachments on `UserMessage` rather than empty blocks. | Stderr report notes attachment count. |
| **Unlabelled Thinking Blocks** | Imported without assigning `ReasoningProvenance::Native` (avoids false claims of native model thought). | Imported with conservative / unlabelled provenance. |
| **Truncated Tool Output** | Pi `truncated: true` and dropped byte count are folded into text representation. | Preserved in text content. |

---

## 4. `pi-rs` -> Pi (`pi-rs export`)

`pi-rs export <session-id> --config <file> [--out <path>]` exports a `pi-rs` session in Pi's JSONL format.

### 4.1 Mapped Fields

- **Header**: Version 3 session header with session id, working directory, and start timestamp.
- **Linear Conversation**: Alternating user and assistant message entries linked sequentially
  via `parentId`.
- **Assistant Prose & Tool Calls**: Folded from streamed deltas into Pi message content blocks.

### 4.2 Dropped Metadata on Export

| `pi-rs` Subsystem | Metadata Dropped on Export | Rationale & Surfacing |
|---|---|---|
| **Reasoning Provenance** | 4-tier provenance (`Native`, `ProviderSummary`, `Declared`, `Reconstructed`) | Pi has thinking blocks but no provenance field. Emits stderr warning: `dropped: reasoning (<provenance>): <chars> chars in <chunks> chunk(s)`. |
| **Tool Execution Lifecycle** | Intermediate states (`ToolStarted`, `ToolUnknown`, `ToolFailed`), duration, and `read_only` invariants | Pi records only user/assistant/toolResult messages. Tool lifecycle detail remains in `trace.jsonl`. Emits stderr count. |
| **Model Epochs & Failovers** | `ModelEpochStarted`, `ModelFailover` reasons (timeout, rate limit, quota, context overflow), capability sets | Pi has only string `model` fields. Epoch boundaries and failover rationales stay in `trace.jsonl`. Emits stderr count. |
| **Blob Store References** | Content-addressed `blobs/sha256/...` paths and typed `ExternalizedField` records | Pi files do not support external content-addressed blob stores. The inline preview string is exported. |
| **Context Reduction** | `ContextReduced` events, before/after token/byte ratios, and recovery pointers | Pi session format has no representation for context budget management. Emits stderr count. |
| **Durable Redactions** | `redactions` count per journal line and secret masking audit trail | Export contains redacted strings, but redaction counters and policies are dropped. |
| **Diagnostics & Checkpoints** | Informational, warning, and error `Diagnostic` events, and filesystem `Checkpoint` paths | Non-message agent events have no counterpart in Pi message format. Emits stderr count. |
| **Session End Reason** | `SessionEnded` structured reason (`Clean`, `FaultRecoveryExhausted`, `UserInterrupted`, `ProviderError`) | Pi session terminates at EOF without terminal status records. Emits stderr count. |

---

## 5. Summary Matrix: Bidirectional Fidelity

| Feature / Metadata | In `pi-rs` Trace | In Pi JSONL | Round-Trip Status |
|---|---|---|---|
| User & Assistant text | Yes | Yes | **Full fidelity** |
| Tool invocation arguments | Yes | Yes | **Full fidelity** (inline or preview) |
| Tool completion results | Yes | Yes | **Full fidelity** (inline or preview) |
| Conversation turn order | Yes | Yes | **Full fidelity** (linear path) |
| Session CWD and timestamps | Yes | Yes | **Full fidelity** |
| Reasoning text | Yes | Yes | **Partial** (text preserved, provenance dropped) |
| Reasoning provenance | Yes | No | **Non-round-trippable** (named on stderr) |
| Multi-branch trees | No (active path) | Yes | **Non-round-trippable** (off-path dropped) |
| Tool lifecycle states | Yes | No | **Non-round-trippable** (trace only) |
| Model epochs & failovers | Yes | No | **Non-round-trippable** (trace only) |
| Blob store externalization | Yes | No | **Non-round-trippable** (trace only) |
| Context reductions | Yes | No | **Non-round-trippable** (trace only) |
| Diagnostics & Checkpoints | Yes | No | **Non-round-trippable** (trace only) |
| Usage cache / billing cost | No | Yes | **Non-round-trippable** (ignored on import) |
| Compaction summary text | No (marker only) | Yes | **Non-round-trippable** (prevent duplicate turns) |
| Extension-specific entries | No | Yes | **Non-round-trippable** (reported on import) |
