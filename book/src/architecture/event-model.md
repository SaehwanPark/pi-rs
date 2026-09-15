# Typed Event Model

Execution in `pi-rs` is recorded as a continuous stream of strongly typed events defined by the `AgentEvent` enum in `crates/pi-rs-core/src/event.rs`.

---

## Event Categories

Every runtime occurrence falls into one of these canonical families:

### 1. Session Lifecycle
- `SessionStarted`: Emitted when a session begins, capturing session ID, active model name, and initial provenance capability.
- `SessionEnded`: Normal or abnormal session termination.
- `TurnStarted` / `TurnCompleted`: Demarcates an autonomous turn boundary.

### 2. Model & Generation
- `ModelRequested`: Outgoing prompt parameters, temperature, max tokens.
- `ModelDelta`: Streamed content chunk containing assistant prose.
- `ReasoningDelta`: Streamed thinking chunk with tagged `ProvenanceKind`.
- `ModelCompleted`: Completion metadata, token usage, latency metrics.

### 3. Tool Lifecycle
Every tool execution produces four explicit events:
1. `ToolRequested`: Model generated a call request with arguments.
2. `ToolStarted`: Local runtime verified confinement and initiated execution.
3. `ToolCompleted`: Tool returned successfully with output payload.
4. `ToolFailed`: Execution failed or was refused (e.g. mutating action disallowed).

### 4. Context & Compaction
- `ContextCompacted`: Context window reached threshold; working set was pruned or summarized while preserving the underlying canonical trace.
- `CheckpointSaved`: Semantic snapshot recorded for fast session resumption.

### 5. Failover & Recovery
- `FailoverAttempted`: Primary model encountered an error; evaluating backup capability.
- `FailoverSucceeded`: Backup model took over active processing.
- `ModelEpochSwitched`: Monotonically increasing epoch number incremented, binding subsequent turns to the new provider.
