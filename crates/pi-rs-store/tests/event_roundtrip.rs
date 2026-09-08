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
  event::{
    AgentEvent, AssistantDelta, EventEnvelope, EventMeta, ModelEpochStarted, ModelFailover,
    ModelRequestCompleted, ModelRequestStarted, ModelRetry, ReasoningDelta, SessionStarted,
    ToolRequested, UserMessage,
  },
  failure::ModelFailureKind,
  ids::{EventId, SessionId, ToolCallId, TraceId, TurnId},
  provenance::ReasoningProvenance,
  session::{SESSION_SCHEMA_VERSION, SessionHeader},
  trace::TraceEntry,
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
  let mut envelope = EventEnvelope::new(meta(&session_id), event);
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
    call_id: ToolCallId::from_string("55555555-5555-4555-8555-555555555555"),
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
  assert_eq!(
    body.call_id,
    ToolCallId::from_string("55555555-5555-4555-8555-555555555555")
  );
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
