//! Every `AgentEvent` variant must survive a store round-trip.
//!
//! The canonical trace is the runtime's durable memory, so the store's contract
//! is not "it accepts an envelope" but "the envelope that comes back is the
//! envelope that went in". This file covers one variant per test on purpose: a
//! single table-driven test stops at the first variant that fails and hides the
//! rest, and a variant that does *not* round-trip is the finding.
//!
//! Path under test:
//!
//! ```text
//! Store::open -> Session::emit -> drop writers
//!   -> Store::open again -> TraceJournal::read  (the event itself)
//!   -> Store::restore                           (the position it promises)
//! ```
//!
//! `Store::restore` returns messages, epochs, compactions, and the last sequence
//! number; it deliberately does not return trace events, because resume cost is
//! meant to be bounded by the checkpoint barrier rather than by trace length. So
//! event identity is proven by reading the trace journal `emit` wrote, and
//! `restore` is checked for the position it does claim.
//!
//! Text is neutral prose throughout. The default write policy redacts at the
//! write boundary, and these tests are about variant fidelity, not redaction; a
//! string that looks like a credential would test the wrong layer.

use std::path::PathBuf;

use pi_rs_core::{
  capability::{CapabilityGap, EpochReason, ModelCapabilities, ModelRef, ReasoningExposure},
  context::{ContextLevel, ReductionReason},
  event::{
    AgentEvent, AssistantDelta, CheckpointCreated, ContextCompactionCompleted,
    ContextCompactionStarted, ContextReduced, Diagnostic, DiagnosticLevel, EventEnvelope,
    EventMeta, ExternalContextRetrieved, ModelEpochStarted, ModelFailover, ModelRequestCompleted,
    ModelRequestStarted, ModelRetry, ReasoningDelta, SessionEndReason, SessionEnded,
    SessionStarted, ToolCompleted, ToolFailed, ToolRequested, ToolStarted, ToolUnknown,
    TurnCompleted, TurnStatus, UserMessage,
  },
  failure::ModelFailureKind,
  ids::{CheckpointId, EventId, EventSeq, SessionId, ToolCallId, TraceId, TurnId},
  provenance::ReasoningProvenance,
  session::{SESSION_SCHEMA_VERSION, SessionHeader},
  tool::ToolExecutionState,
  trace::{BlobRef, ExternalContextSource, TraceEntry},
};
use pi_rs_store::{Store, TraceJournal, WritePolicy, tmp::TempDir};

fn model() -> ModelRef {
  ModelRef::new("local", "qwen")
}

fn capabilities() -> ModelCapabilities {
  ModelCapabilities {
    text: true,
    images: false,
    tools: true,
    exposed_reasoning: ReasoningExposure::Native,
    context_window: 32_768,
    max_output_tokens: Some(4_096),
  }
}

/// The tool call id every tool-lifecycle variant shares, spelled once so a
/// test cannot quietly compare a start against a different call than it ended.
fn tool_call_id() -> ToolCallId {
  ToolCallId::from_string("55555555-5555-4555-8555-555555555555")
}

fn header(session: &SessionId) -> SessionHeader {
  SessionHeader {
    session_id: session.clone(),
    version: SESSION_SCHEMA_VERSION,
    started_at_ms: 1_700_000_000_000,
    working_dir: "/repo".into(),
    model: model(),
    parent_session: None,
    branched_from_event: None,
    imported_from: None,
  }
}

/// Metadata fixed on purpose: a round-trip that depended on wall-clock time or
/// on freshly minted identifiers could not tell "restored" apart from "rebuilt".
fn meta(session: &SessionId) -> EventMeta {
  EventMeta {
    event_id: EventId::from_string("11111111-1111-4111-8111-111111111111"),
    session_id: session.clone(),
    turn_id: Some(TurnId::from_string("22222222-2222-4222-8222-222222222222")),
    seq: None,
    timestamp_ms: 1_700_000_000_001,
    model_epoch: Some(0),
    model: Some(model()),
    tool_call_id: None,
    parent_event_id: None,
    trace_id: TraceId::from_string("33333333-3333-4333-8333-333333333333"),
    span_id: pi_rs_core::ids::SpanId::from_string("44444444-4444-4444-8444-444444444444"),
  }
}

/// The shared metadata, with the per-emission identity fields replaced.
///
/// Multi-event tests need distinct identities or they cannot tell one line from
/// the next; the fixed values in `meta` stay, so timestamps and attribution are
/// still not what a comparison depends on.
fn meta_at(session: &SessionId, index: usize) -> EventMeta {
  let mut meta = meta(session);
  meta.event_id = EventId::from_string(format!("event-{index:04}"));
  meta.span_id = pi_rs_core::ids::SpanId::from_string(format!("span-{index:04}"));
  meta
}

/// The discriminant name, taken from the value rather than from a hand-written
/// string that can drift away from the variant it labels.
fn variant(event: &AgentEvent) -> String {
  format!("{event:?}")
    .split('(')
    .next()
    .unwrap_or("AgentEvent")
    .to_string()
}

/// Append one event through the store, then read it back from a reopened store.
///
/// The writers are dropped before reading so the read path cannot be served by
/// a handle that still holds the event in memory. The invariants every variant
/// shares (exactly one line, nothing malformed, the sequence the store assigned
/// is the sequence that comes back, whole-envelope equality) are asserted here;
/// each test then asserts what only its own variant carries.
fn round_trip(event: AgentEvent) -> TraceEntry {
  let name = variant(&event);
  let dir = TempDir::new("event-roundtrip");
  let session_id = SessionId::new();

  let store = Store::open(dir.path(), WritePolicy::default()).expect("open the store");
  let mut session = store
    .begin(header(&session_id))
    .expect("begin a durable session");
  let mut envelope = EventEnvelope::new(meta_at(&session_id, 0), event);
  let seq = session
    .emit(&mut envelope)
    .unwrap_or_else(|error| panic!("{name}: emit must be accepted, got {error}"));
  assert_eq!(
    envelope.meta.seq,
    Some(seq),
    "{name}: emit must stamp the assigned sequence back into the caller's envelope"
  );
  let emitted = envelope.clone();
  let written_trace = session.trace_path().to_path_buf();
  drop(session);
  drop(store);

  let reopened =
    Store::open(dir.path(), WritePolicy::default()).expect("reopen the store after the writers");
  let trace = trace_path(&reopened, &session_id);
  assert_eq!(
    written_trace, trace,
    "{name}: the trace the session wrote is the trace the layout resolves"
  );

  let report = TraceJournal::read(&trace)
    .unwrap_or_else(|error| panic!("{name}: the trace line must decode, got {error}"));
  assert_eq!(
    report.malformed, 0,
    "{name}: no line may be unreadable, first bad line is {:?}",
    report.first_malformed_line
  );
  assert_eq!(
    report.items.len(),
    1,
    "{name}: appending one event must produce exactly one trace line"
  );
  let entry = report.items.into_iter().next().expect("one trace line");

  assert_eq!(
    entry.envelope, emitted,
    "{name}: the restored envelope must equal the emitted envelope"
  );
  assert_eq!(
    entry.envelope.meta.seq,
    Some(seq),
    "{name}: the sequence number must survive the line"
  );
  assert_eq!(
    entry.redactions, 0,
    "{name}: neutral prose must not be redacted, or this test is measuring redaction"
  );

  let restored = Store::open(dir.path(), WritePolicy::default())
    .expect("open the store to restore")
    .restore(&session_id)
    .unwrap_or_else(|error| panic!("{name}: restore must succeed, got {error}"));
  assert_eq!(
    restored.last_seq,
    Some(seq),
    "{name}: restore must recover the journal position without hydrating the trace"
  );
  assert_eq!(
    restored.total_records, 1,
    "{name}: emitting a trace event must not fabricate a session record"
  );
  assert!(
    restored.messages.is_empty(),
    "{name}: the trace journal is not the model-visible message list"
  );
  assert!(
    restored.checkpoint.is_none(),
    "{name}: appending one event must not create a checkpoint barrier"
  );

  entry
}

/// Fail with the variant named when the store hands back a different variant.
///
/// Written as a helper so every test reports the same shape of failure: which
/// variant was asked for, and what actually came out of the store.
fn assert_same_variant(restored: &AgentEvent, expected: &AgentEvent) {
  assert_eq!(
    std::mem::discriminant(restored),
    std::mem::discriminant(expected),
    "{}: the store must give back the same variant, got {restored:?}",
    variant(expected)
  );
}

fn trace_path(store: &Store, session: &SessionId) -> PathBuf {
  store.layout().trace_path(session)
}

#[test]
fn session_started_round_trips() {
  let original = AgentEvent::SessionStarted(SessionStarted {
    working_dir: "/repo/worktrees/pi-rs-rt".into(),
    model: model(),
    capabilities: capabilities(),
    resumed: false,
  });
  let entry = round_trip(original.clone());
  let restored = &entry.envelope.event;

  assert_same_variant(restored, &original);
  let AgentEvent::SessionStarted(body) = restored else {
    unreachable!("assert_same_variant already proved the discriminant");
  };
  assert_eq!(body.working_dir, "/repo/worktrees/pi-rs-rt");
  assert_eq!(body.model, model());
  assert_eq!(body.capabilities, capabilities());
  assert!(
    !body.resumed,
    "resumed must stay false rather than defaulting from an absent field"
  );
}

#[test]
fn user_message_round_trips() {
  let original = AgentEvent::UserMessage(UserMessage {
    text: "rename the journal writer and keep the ordering rule".into(),
    attachments: 2,
  });
  let entry = round_trip(original.clone());
  let restored = &entry.envelope.event;

  assert_same_variant(restored, &original);
  let AgentEvent::UserMessage(body) = restored else {
    unreachable!("assert_same_variant already proved the discriminant");
  };
  assert_eq!(
    body.text,
    "rename the journal writer and keep the ordering rule"
  );
  assert_eq!(
    body.attachments, 2,
    "a non-text block count must not collapse to zero"
  );
}

#[test]
fn model_request_started_round_trips() {
  let original = AgentEvent::ModelRequestStarted(ModelRequestStarted {
    epoch: 3,
    model: ModelRef::new("openai-codex", "gpt-5.6-luna"),
    message_count: 17,
    context_tokens_est: 12_345,
    tools_exposed: 9,
  });
  let entry = round_trip(original.clone());
  let restored = &entry.envelope.event;

  assert_same_variant(restored, &original);
  let AgentEvent::ModelRequestStarted(body) = restored else {
    unreachable!("assert_same_variant already proved the discriminant");
  };
  assert_eq!(body.epoch, 3);
  assert_eq!(
    body.model,
    ModelRef::new("openai-codex", "gpt-5.6-luna"),
    "provider/model is the attribution key, it must not be split or folded"
  );
  assert_eq!(body.message_count, 17);
  assert_eq!(body.context_tokens_est, 12_345);
  assert_eq!(body.tools_exposed, 9);
}

#[test]
fn reasoning_delta_round_trips() {
  let original = AgentEvent::ReasoningDelta(ReasoningDelta {
    text: "weighing the narrow edit against the broad rewrite".into(),
    provenance: ReasoningProvenance::ProviderSummary,
    chunk_index: 7,
  });
  let entry = round_trip(original.clone());
  let restored = &entry.envelope.event;

  assert_same_variant(restored, &original);
  let AgentEvent::ReasoningDelta(body) = restored else {
    unreachable!("assert_same_variant already proved the discriminant");
  };
  assert_eq!(
    body.text,
    "weighing the narrow edit against the broad rewrite"
  );
  assert_eq!(
    body.provenance,
    ReasoningProvenance::ProviderSummary,
    "provenance must never be coerced to native on the way through the store"
  );
  assert_eq!(body.chunk_index, 7);
}

#[test]
fn assistant_delta_round_trips() {
  let original = AgentEvent::AssistantDelta(AssistantDelta {
    text: "the store appends one compact json line".into(),
    chunk_index: 42,
  });
  let entry = round_trip(original.clone());
  let restored = &entry.envelope.event;

  assert_same_variant(restored, &original);
  let AgentEvent::AssistantDelta(body) = restored else {
    unreachable!("assert_same_variant already proved the discriminant");
  };
  assert_eq!(body.text, "the store appends one compact json line");
  assert_eq!(body.chunk_index, 42);
}

#[test]
fn model_request_completed_round_trips() {
  let original = AgentEvent::ModelRequestCompleted(ModelRequestCompleted {
    epoch: 1,
    model: model(),
    finish_reason: Some("tool_calls".into()),
    input_tokens: Some(8_123),
    output_tokens: Some(4_567),
    duration_ms: 9_012,
    tool_calls: 2,
    reasoning_provenance: Some(ReasoningProvenance::Native),
  });
  let entry = round_trip(original.clone());
  let restored = &entry.envelope.event;

  assert_same_variant(restored, &original);
  let AgentEvent::ModelRequestCompleted(body) = restored else {
    unreachable!("assert_same_variant already proved the discriminant");
  };
  assert_eq!(body.epoch, 1);
  assert_eq!(body.model, model());
  assert_eq!(body.finish_reason.as_deref(), Some("tool_calls"));
  assert_eq!(body.input_tokens, Some(8_123));
  assert_eq!(body.output_tokens, Some(4_567));
  assert_eq!(body.duration_ms, 9_012);
  assert_eq!(body.tool_calls, 2);
  assert_eq!(
    body.reasoning_provenance,
    Some(ReasoningProvenance::Native),
    "provenance must survive even when the reasoning deltas themselves were reduced"
  );
}

#[test]
fn model_retry_round_trips() {
  let original = AgentEvent::ModelRetry(ModelRetry {
    attempt: 2,
    max_attempts: 4,
    kind: ModelFailureKind::RateLimited,
    retry_after_ms: Some(750),
    will_failover: true,
  });
  let entry = round_trip(original.clone());
  let restored = &entry.envelope.event;

  assert_same_variant(restored, &original);
  let AgentEvent::ModelRetry(body) = restored else {
    unreachable!("assert_same_variant already proved the discriminant");
  };
  assert_eq!(body.attempt, 2);
  assert_eq!(body.max_attempts, 4);
  assert_eq!(body.kind, ModelFailureKind::RateLimited);
  assert_eq!(body.retry_after_ms, Some(750));
  assert!(
    body.will_failover,
    "an intent to take over next must not default back to false"
  );
}

#[test]
fn model_failover_round_trips() {
  let original = AgentEvent::ModelFailover(ModelFailover {
    from: ModelRef::new("openai-codex", "gpt-5.6-luna"),
    to: ModelRef::new("local-vulcan", "qwen3.8-flash"),
    kind: ModelFailureKind::ProviderUnavailable,
    gaps: vec![
      CapabilityGap::Images,
      CapabilityGap::ContextWindow {
        required: 131_072,
        available: 32_768,
      },
    ],
    compacted: true,
  });
  let entry = round_trip(original.clone());
  let restored = &entry.envelope.event;

  assert_same_variant(restored, &original);
  let AgentEvent::ModelFailover(body) = restored else {
    unreachable!("assert_same_variant already proved the discriminant");
  };
  assert_eq!(body.from, ModelRef::new("openai-codex", "gpt-5.6-luna"));
  assert_eq!(body.to, ModelRef::new("local-vulcan", "qwen3.8-flash"));
  assert_eq!(body.kind, ModelFailureKind::ProviderUnavailable);
  assert_eq!(
    body.gaps,
    vec![
      CapabilityGap::Images,
      CapabilityGap::ContextWindow {
        required: 131_072,
        available: 32_768
      }
    ],
    "a tolerated capability gap is an auditable takeover decision, not decoration"
  );
  assert!(body.compacted, "the rebudget flag must survive");
}

#[test]
fn model_epoch_started_round_trips() {
  let original = AgentEvent::ModelEpochStarted(ModelEpochStarted {
    epoch: 2,
    model: ModelRef::new("antigravity", "gemini-3.8-fresh"),
    reason: EpochReason::AutomaticFailover,
    capabilities: capabilities(),
  });
  let entry = round_trip(original.clone());
  let restored = &entry.envelope.event;

  assert_same_variant(restored, &original);
  let AgentEvent::ModelEpochStarted(body) = restored else {
    unreachable!("assert_same_variant already proved the discriminant");
  };
  assert_eq!(body.epoch, 2);
  assert_eq!(body.model, ModelRef::new("antigravity", "gemini-3.8-fresh"));
  assert_eq!(
    body.reason,
    EpochReason::AutomaticFailover,
    "why an epoch began is the difference between failover and a manual switch"
  );
  assert_eq!(body.capabilities, capabilities());
}

#[test]
fn tool_requested_round_trips() {
  let arguments = serde_json::json!({
    "path": "crates/pi-rs-store/src/journal.rs",
    "offset": 91,
    "limit": 20,
    "nested": { "follow_symlinks": false },
  });
  let original = AgentEvent::ToolRequested(ToolRequested {
    call_id: tool_call_id(),
    name: "read".into(),
    arguments: arguments.clone(),
    read_only: true,
  });
  let entry = round_trip(original.clone());
  let restored = &entry.envelope.event;

  assert_same_variant(restored, &original);
  let AgentEvent::ToolRequested(body) = restored else {
    unreachable!("assert_same_variant already proved the discriminant");
  };
  assert_eq!(body.call_id, tool_call_id());
  assert_eq!(body.name, "read");
  assert_eq!(
    body.arguments, arguments,
    "tool arguments are the replayable request, they must not be summarised"
  );
  assert!(
    body.read_only,
    "the mutation flag is what keeps replay safe"
  );
}

#[test]
fn tool_started_round_trips() {
  let original = AgentEvent::ToolStarted(ToolStarted {
    call_id: tool_call_id(),
    name: "bash".into(),
  });
  let entry = round_trip(original.clone());
  let restored = &entry.envelope.event;

  assert_same_variant(restored, &original);
  let AgentEvent::ToolStarted(body) = restored else {
    unreachable!("assert_same_variant already proved the discriminant");
  };
  assert_eq!(
    body.call_id,
    tool_call_id(),
    "the call id is the only thing joining a start to its completion"
  );
  assert_eq!(body.name, "bash");
}

#[test]
fn tool_completed_round_trips() {
  let blob = BlobRef::for_bytes(b"whole tool output bytes".as_slice(), Some("text/plain"));
  let original = AgentEvent::ToolCompleted(ToolCompleted {
    call_id: tool_call_id(),
    name: "read".into(),
    state: ToolExecutionState::Succeeded,
    duration_ms: 41,
    status: Some(0),
    reduced: true,
    blob: Some(blob.clone()),
    visible_bytes: 1_024,
  });
  let entry = round_trip(original.clone());
  let restored = &entry.envelope.event;

  assert_same_variant(restored, &original);
  let AgentEvent::ToolCompleted(body) = restored else {
    unreachable!("assert_same_variant already proved the discriminant");
  };
  assert_eq!(body.call_id, tool_call_id());
  assert_eq!(body.name, "read");
  assert_eq!(body.state, ToolExecutionState::Succeeded);
  assert_eq!(body.duration_ms, 41);
  assert_eq!(
    body.status,
    Some(0),
    "an exit status of zero is a fact, not an absence"
  );
  assert!(body.reduced);
  assert_eq!(
    body.blob.as_ref(),
    Some(&blob),
    "the pointer back to the full payload must survive, recovery depends on it"
  );
  assert_eq!(body.visible_bytes, 1_024);
}

#[test]
fn tool_failed_round_trips() {
  let original = AgentEvent::ToolFailed(ToolFailed {
    call_id: tool_call_id(),
    name: "bash".into(),
    message: "command exited with a non-zero status".into(),
    duration_ms: 613,
    status: Some(127),
  });
  let entry = round_trip(original.clone());
  let restored = &entry.envelope.event;

  assert_same_variant(restored, &original);
  let AgentEvent::ToolFailed(body) = restored else {
    unreachable!("assert_same_variant already proved the discriminant");
  };
  assert_eq!(body.call_id, tool_call_id());
  assert_eq!(body.name, "bash");
  assert_eq!(body.message, "command exited with a non-zero status");
  assert_eq!(body.duration_ms, 613);
  assert_eq!(body.status, Some(127));
}

#[test]
fn tool_unknown_round_trips() {
  let original = AgentEvent::ToolUnknown(ToolUnknown {
    call_id: tool_call_id(),
    name: "write".into(),
    why: "the process exited before completion was observed".into(),
    mutating: true,
  });
  let entry = round_trip(original.clone());
  let restored = &entry.envelope.event;

  assert_same_variant(restored, &original);
  let AgentEvent::ToolUnknown(body) = restored else {
    unreachable!("assert_same_variant already proved the discriminant");
  };
  assert_eq!(body.call_id, tool_call_id());
  assert_eq!(body.name, "write");
  assert_eq!(
    body.why, "the process exited before completion was observed",
    "the unobserved boundary has to be readable later, not just countable"
  );
  assert!(
    body.mutating,
    "unknown must never be coerced into success or failure, especially when mutating"
  );
}

#[test]
fn external_context_retrieved_round_trips() {
  let original = AgentEvent::ExternalContextRetrieved(ExternalContextRetrieved {
    source: ExternalContextSource {
      provider: "rkb-rs".into(),
      resource_id: "doc/canonical-context".into(),
      provenance: "rkb-rs/citation".into(),
    },
    citation: Some("section 4.2".into()),
    bytes: 8_192,
    inline: false,
  });
  let entry = round_trip(original.clone());
  let restored = &entry.envelope.event;

  assert_same_variant(restored, &original);
  let AgentEvent::ExternalContextRetrieved(body) = restored else {
    unreachable!("assert_same_variant already proved the discriminant");
  };
  assert_eq!(body.source.provider, "rkb-rs");
  assert_eq!(body.source.resource_id, "doc/canonical-context");
  assert_eq!(body.source.provenance, "rkb-rs/citation");
  assert_eq!(body.citation.as_deref(), Some("section 4.2"));
  assert_eq!(body.bytes, 8_192);
  assert!(
    !body.inline,
    "inline versus referenced changes what replay can recover"
  );
}

#[test]
fn context_reduced_round_trips() {
  let blob = BlobRef::for_bytes(b"the full tool output that did not fit".as_slice(), None);
  let original = AgentEvent::ContextReduced(ContextReduced {
    reason: ReductionReason::OversizedToolOutput { limit_bytes: 8_192 },
    original_bytes: 40_960,
    visible_bytes: 7_900,
    blob: blob.clone(),
    recovery_ref: "blobs/3f9a1c7e2b04".into(),
    tool_call_id: Some(tool_call_id()),
  });
  let entry = round_trip(original.clone());
  let restored = &entry.envelope.event;

  assert_same_variant(restored, &original);
  let AgentEvent::ContextReduced(body) = restored else {
    unreachable!("assert_same_variant already proved the discriminant");
  };
  assert_eq!(
    body.reason,
    ReductionReason::OversizedToolOutput { limit_bytes: 8_192 },
    "why a payload was reduced is what makes the reduction auditable"
  );
  assert_eq!(body.original_bytes, 40_960);
  assert_eq!(body.visible_bytes, 7_900);
  assert_eq!(body.blob, blob);
  assert_eq!(
    body.recovery_ref, "blobs/3f9a1c7e2b04",
    "recovery must be possible from this string alone"
  );
  assert_eq!(body.tool_call_id, Some(tool_call_id()));
}

#[test]
fn context_compaction_started_round_trips() {
  let original = AgentEvent::ContextCompactionStarted(ContextCompactionStarted {
    level: ContextLevel::L1Ordinary,
    reason: "recent window exceeded its target".into(),
  });
  let entry = round_trip(original.clone());
  let restored = &entry.envelope.event;

  assert_same_variant(restored, &original);
  let AgentEvent::ContextCompactionStarted(body) = restored else {
    unreachable!("assert_same_variant already proved the discriminant");
  };
  assert_eq!(
    body.level,
    ContextLevel::L1Ordinary,
    "the level decides whether a safe boundary was required, so it cannot blur"
  );
  assert_eq!(body.reason, "recent window exceeded its target");
}

#[test]
fn context_compaction_completed_round_trips() {
  let original = AgentEvent::ContextCompactionCompleted(ContextCompactionCompleted {
    level: ContextLevel::L2Phase,
    removed_messages: 12,
    retained_messages: 30,
    context_epoch: 4,
  });
  let entry = round_trip(original.clone());
  let restored = &entry.envelope.event;

  assert_same_variant(restored, &original);
  let AgentEvent::ContextCompactionCompleted(body) = restored else {
    unreachable!("assert_same_variant already proved the discriminant");
  };
  assert_eq!(body.level, ContextLevel::L2Phase);
  assert_eq!(body.removed_messages, 12);
  assert_eq!(body.retained_messages, 30);
  assert_eq!(
    body.context_epoch, 4,
    "a compaction boundary the next request must be attributed to"
  );
}

#[test]
fn checkpoint_created_round_trips() {
  let original = AgentEvent::CheckpointCreated(CheckpointCreated {
    checkpoint_id: CheckpointId::from_string("66666666-6666-4666-8666-666666666666"),
    capsule_version: 1,
    summarized_events: 250,
    path: "checkpoints/66666666-6666-4666-8666-666666666666.json".into(),
  });
  let entry = round_trip(original.clone());
  let restored = &entry.envelope.event;

  assert_same_variant(restored, &original);
  let AgentEvent::CheckpointCreated(body) = restored else {
    unreachable!("assert_same_variant already proved the discriminant");
  };
  assert_eq!(
    body.checkpoint_id,
    CheckpointId::from_string("66666666-6666-4666-8666-666666666666")
  );
  assert_eq!(body.capsule_version, 1);
  assert_eq!(body.summarized_events, 250);
  assert_eq!(
    body.path, "checkpoints/66666666-6666-4666-8666-666666666666.json",
    "the capsule path is how a reader finds what the barrier summarized"
  );
}

#[test]
fn turn_completed_round_trips() {
  let original = AgentEvent::TurnCompleted(TurnCompleted {
    status: TurnStatus::Completed,
    duration_ms: 1_890,
  });
  let entry = round_trip(original.clone());
  let restored = &entry.envelope.event;

  assert_same_variant(restored, &original);
  let AgentEvent::TurnCompleted(body) = restored else {
    unreachable!("assert_same_variant already proved the discriminant");
  };
  assert_eq!(body.status, TurnStatus::Completed);
  assert_eq!(body.duration_ms, 1_890);
}

#[test]
fn diagnostic_round_trips() {
  let original = AgentEvent::Diagnostic(Diagnostic {
    level: DiagnosticLevel::Warn,
    message: "the retry budget was spent before a usable answer".into(),
  });
  let entry = round_trip(original.clone());
  let restored = &entry.envelope.event;

  assert_same_variant(restored, &original);
  let AgentEvent::Diagnostic(body) = restored else {
    unreachable!("assert_same_variant already proved the discriminant");
  };
  assert_eq!(body.level, DiagnosticLevel::Warn);
  assert_eq!(
    body.message, "the retry budget was spent before a usable answer",
    "a diagnostic is the only record of a rare event once the status line moved on"
  );
}

#[test]
fn session_ended_round_trips() {
  let original = AgentEvent::SessionEnded(SessionEnded {
    reason: SessionEndReason::UserExit,
  });
  let entry = round_trip(original.clone());
  let restored = &entry.envelope.event;

  assert_same_variant(restored, &original);
  let AgentEvent::SessionEnded(body) = restored else {
    unreachable!("assert_same_variant already proved the discriminant");
  };
  assert_eq!(body.reason, SessionEndReason::UserExit);
}

#[test]
fn every_variant_round_trips_in_one_session_in_order() {
  let events = all_variants();
  let dir = TempDir::new("event-roundtrip-all");
  let session_id = SessionId::new();

  let store = Store::open(dir.path(), WritePolicy::default()).expect("open the store");
  let mut session = store
    .begin(header(&session_id))
    .expect("begin a durable session");
  let mut emitted = Vec::new();
  for (index, event) in events.iter().enumerate() {
    let mut envelope = EventEnvelope::new(meta_at(&session_id, index), event.clone());
    session
      .emit(&mut envelope)
      .unwrap_or_else(|error| panic!("emit variant {index}, got {error}"));
    assert_eq!(
      envelope.meta.seq,
      Some(EventSeq((index + 1) as u64)),
      "variant {index} must be assigned the next sequence number"
    );
    emitted.push(envelope);
  }
  let trace = session.trace_path().to_path_buf();
  drop(session);
  drop(store);

  let reopened =
    Store::open(dir.path(), WritePolicy::default()).expect("reopen the store after the writers");
  let report = TraceJournal::read(&trace).expect("read the trace back");
  assert_eq!(
    report.malformed, 0,
    "no line of a mixed journal may be unreadable"
  );
  assert_eq!(
    report.items.len(),
    events.len(),
    "every variant must contribute exactly one line"
  );
  for (index, entry) in report.items.iter().enumerate() {
    assert_eq!(
      entry.envelope,
      emitted[index],
      "line {} must come back unchanged",
      index + 1
    );
    assert_eq!(
      entry.envelope.meta.seq,
      Some(EventSeq((index + 1) as u64)),
      "line {} must keep the position the journal assigned it",
      index + 1
    );
    assert_eq!(
      std::mem::discriminant(&entry.envelope.event),
      std::mem::discriminant(&events[index]),
      "line {} must keep its variant, got {:?}",
      index + 1,
      entry.envelope.event
    );
  }

  let restored = reopened
    .restore(&session_id)
    .expect("restore the mixed session");
  assert_eq!(
    restored.last_seq,
    Some(EventSeq(events.len() as u64)),
    "restore must land on the last line of a journal that mixes every variant"
  );
}

#[test]
fn generated_variant_list_has_a_case_for_every_variant() {
  let list = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("../../docs/EVENT_VARIANTS.txt")
    .canonicalize()
    .expect("docs/EVENT_VARIANTS.txt is the list this suite is written against");
  let documented: Vec<String> = std::fs::read_to_string(list)
    .expect("read the generated variant list")
    .lines()
    .map(str::trim)
    .filter(|line| !line.is_empty())
    .map(str::to_string)
    .collect();
  let covered: Vec<String> = all_variants().iter().map(variant).collect();

  assert_eq!(
    covered, documented,
    "the suite must cover the generated list exactly: missing or extra variants"
  );
  assert_eq!(
    covered.len(),
    covered
      .as_slice()
      .iter()
      .collect::<std::collections::HashSet<_>>()
      .len(),
    "a variant must not be counted twice under two different spellings"
  );
}

/// One value per `AgentEvent` variant, in the order the generated list names
/// them. This is the enumeration the in-order journal test replays and the
/// list test compares against, so a new variant cannot be added to the enum
/// without this file noticing.
fn all_variants() -> Vec<AgentEvent> {
  vec![
    AgentEvent::SessionStarted(SessionStarted {
      working_dir: "/repo".into(),
      model: model(),
      capabilities: capabilities(),
      resumed: true,
    }),
    AgentEvent::UserMessage(UserMessage {
      text: "list the durable events".into(),
      attachments: 1,
    }),
    AgentEvent::ModelRequestStarted(ModelRequestStarted {
      epoch: 0,
      model: model(),
      message_count: 2,
      context_tokens_est: 512,
      tools_exposed: 4,
    }),
    AgentEvent::ReasoningDelta(ReasoningDelta {
      text: "checking the ordering rule".into(),
      provenance: ReasoningProvenance::Native,
      chunk_index: 0,
    }),
    AgentEvent::AssistantDelta(AssistantDelta {
      text: "here is the answer".into(),
      chunk_index: 0,
    }),
    AgentEvent::ModelRequestCompleted(ModelRequestCompleted {
      epoch: 0,
      model: model(),
      finish_reason: Some("stop".into()),
      input_tokens: Some(10),
      output_tokens: Some(4),
      duration_ms: 25,
      tool_calls: 0,
      reasoning_provenance: Some(ReasoningProvenance::Native),
    }),
    AgentEvent::ModelRetry(ModelRetry {
      attempt: 1,
      max_attempts: 3,
      kind: ModelFailureKind::Transport,
      retry_after_ms: None,
      will_failover: false,
    }),
    AgentEvent::ModelFailover(ModelFailover {
      from: ModelRef::new("openai-codex", "gpt-5.6-luna"),
      to: model(),
      kind: ModelFailureKind::Timeout,
      gaps: vec![CapabilityGap::Tools],
      compacted: false,
    }),
    AgentEvent::ModelEpochStarted(ModelEpochStarted {
      epoch: 1,
      model: model(),
      reason: EpochReason::ManualSwitch,
      capabilities: capabilities(),
    }),
    AgentEvent::ToolRequested(ToolRequested {
      call_id: tool_call_id(),
      name: "read".into(),
      arguments: serde_json::json!({ "path": "docs/SLICE_RT.md" }),
      read_only: true,
    }),
    AgentEvent::ToolStarted(ToolStarted {
      call_id: tool_call_id(),
      name: "read".into(),
    }),
    AgentEvent::ToolCompleted(ToolCompleted {
      call_id: tool_call_id(),
      name: "read".into(),
      state: ToolExecutionState::Succeeded,
      duration_ms: 3,
      status: Some(0),
      reduced: false,
      blob: None,
      visible_bytes: 64,
    }),
    AgentEvent::ToolFailed(ToolFailed {
      call_id: tool_call_id(),
      name: "read".into(),
      message: "no such file".into(),
      duration_ms: 1,
      status: None,
    }),
    AgentEvent::ToolUnknown(ToolUnknown {
      call_id: tool_call_id(),
      name: "write".into(),
      why: "completion was never observed".into(),
      mutating: true,
    }),
    AgentEvent::ExternalContextRetrieved(ExternalContextRetrieved {
      source: ExternalContextSource {
        provider: "web".into(),
        resource_id: "example/doc".into(),
        provenance: "web".into(),
      },
      citation: None,
      bytes: 512,
      inline: true,
    }),
    AgentEvent::ContextReduced(ContextReduced {
      reason: ReductionReason::RecentTargetExceeded {
        target_tokens: 4_000,
      },
      original_bytes: 9_000,
      visible_bytes: 3_000,
      blob: BlobRef::for_bytes(b"reduced payload".as_slice(), None),
      recovery_ref: "blobs/000000000000".into(),
      tool_call_id: None,
    }),
    AgentEvent::ContextCompactionStarted(ContextCompactionStarted {
      level: ContextLevel::L0Payload,
      reason: "one oversized tool output".into(),
    }),
    AgentEvent::ContextCompactionCompleted(ContextCompactionCompleted {
      level: ContextLevel::L1Ordinary,
      removed_messages: 3,
      retained_messages: 9,
      context_epoch: 1,
    }),
    AgentEvent::CheckpointCreated(CheckpointCreated {
      checkpoint_id: CheckpointId::from_string("66666666-6666-4666-8666-666666666666"),
      capsule_version: 1,
      summarized_events: 8,
      path: "checkpoints/66666666-6666-4666-8666-666666666666.json".into(),
    }),
    AgentEvent::TurnCompleted(TurnCompleted {
      status: TurnStatus::Cancelled,
      duration_ms: 700,
    }),
    AgentEvent::Diagnostic(Diagnostic {
      level: DiagnosticLevel::Error,
      message: "the provider closed the stream early".into(),
    }),
    AgentEvent::SessionEnded(SessionEnded {
      reason: SessionEndReason::Restart,
    }),
  ]
}
