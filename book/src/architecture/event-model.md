# Typed Event Model

Execution in `rupi` is recorded as a continuous stream of strongly typed events defined
by the `AgentEvent` enum in `crates/rupi-core/src/event.rs`. The durable journal wraps
each event in an envelope containing session/turn identity, sequence ordering, model
attribution, and trace/span ids.

## Event categories

The current event vocabulary is:

### Session and turn lifecycle

- `SessionStarted`: establishes the session and initial model epoch.
- `UserMessage`: records accepted user input in canonical history.
- `TurnCompleted`: closes a turn as completed, cancelled, or failed.
- `SessionEnded`: records why the session closed.
- `Diagnostic`: records an operator-visible condition without pretending it is assistant text.

### Model requests and recovery

- `ModelRequestStarted`: opens a provider request span.
- `ReasoningDelta`: records reasoning-like text with explicit provenance.
- `AssistantDelta`: records streamed assistant prose.
- `ModelRequestCompleted`: closes the request with usage, finish, and attribution data.
- `ModelRetry`: records a bounded retry against the same model.
- `ModelFailover`: records an availability-driven transition to a backup model.
- `ModelEpochStarted`: snapshots the model, provider, capabilities, and epoch reason.

### Tool lifecycle

A decoded tool call progresses through explicit states rather than an implicit
success/failure assumption:

1. `ToolRequested` — the model supplied a stable call id, name, and arguments.
2. `ToolStarted` — execution crossed the observed start boundary.
3. `ToolCompleted` — a result was committed; output may reference a bounded blob.
4. `ToolFailed` — execution ended with an observed error or refusal.
5. `ToolUnknown` — completion could not be observed. This is not a failure and is a
   reconciliation barrier for mutating operations.

### Context and external evidence

- `ExternalContextRetrieved`: records cited external context and its provenance.
- `ContextReduced`: records bounded model-visible payload reduction while preserving
  canonical evidence.
- `ContextCompactionStarted`: opens an L1/L2 compaction at a safe boundary.
- `ContextSummary`: attributes the summary message that replaces a compacted range.
- `ContextCompactionEpoch`: records the canonical range and model-visible replacement.
- `ContextCompactionCompleted`: closes L1/L2 compaction and records retained counts.
- `CheckpointCreated`: records a durable L3 structured capsule and checkpoint barrier.

## Ordering and replay

The store assigns monotonically increasing `seq` values when events are appended.
Timestamps support human-facing timelines but do not define order. Replay consumes the
same canonical events without starting a provider or executing a recorded tool. Context
compaction changes only the model-visible projection; it never deletes canonical trace
records.

Every event retains the provenance and model epoch that produced it. The renderer may
hide routine events by default, but a quiet transcript does not mean the event was not
recorded.