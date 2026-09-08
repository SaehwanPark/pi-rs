//! Typed runtime events.
//!
//! The event stream is the single source of truth shared by the durable trace,
//! the session journal, the UI, replay, failover continuity, and diagnostics.
//! There is deliberately no second, ad hoc logging path for the same facts.
//!
//! Global rules:
//!
//! - **Identity.** Every event carries [`EventMeta`]. Identity fields are
//!   optional only where the fact does not exist yet (for example
//!   `model_epoch` before the first epoch exists).
//! - **Ordering.** Within one session, ordering is defined by [`EventSeq`],
//!   assigned by the durable event log at append time. Timestamps are for
//!   humans and timelines; they never define order, because clocks step.
//! - **Causality.** `parent_event_id` points at the causal predecessor
//!   (for example a tool completion points at its model request), and
//!   `trace_id`/`span_id` group one causal execution.
//! - **Persistence.** Every event here is journal-worthy by default. Events
//!   that are high-volume by nature (deltas) are journaled in the trace and
//!   may be coalesced or blob-reduced by the store; they are never routed to a
//!   separate channel.
//! - **Replay.** Replaying an event stream must reproduce the same visible
//!   state, including model attribution and reasoning provenance, without
//!   re-executing anything. Events are facts about the past; they never carry
//!   instructions.
//! - **UI relevance.** Rendering is derived from these events only. The UI
//!   does not own runtime state and does not receive facts that events do not
//!   carry.

use serde::{Deserialize, Serialize};

use crate::{
  capability::{CapabilityGap, EpochReason, ModelCapabilities, ModelRef},
  context::{ContextLevel, ReductionReason},
  failure::ModelFailureKind,
  ids::{CheckpointId, EventId, EventSeq, SessionId, ToolCallId, TraceId, TurnId, now_millis},
  message::Message,
  provenance::ReasoningProvenance,
  tool::ToolExecutionState,
  trace::{BlobRef, ExternalContextSource},
};

/// Schema version stamped onto every journal line.
pub const EVENT_SCHEMA_VERSION: u32 = 1;

/// Identity and ordering metadata shared by every event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventMeta {
  pub event_id: EventId,
  pub session_id: SessionId,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub turn_id: Option<TurnId>,
  /// Assigned by the durable log. `None` only while an event is in flight.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub seq: Option<EventSeq>,
  pub timestamp_ms: u64,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub model_epoch: Option<u32>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub model: Option<ModelRef>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub tool_call_id: Option<ToolCallId>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub parent_event_id: Option<EventId>,
  pub trace_id: TraceId,
  pub span_id: crate::ids::SpanId,
}

impl EventMeta {
  pub fn new(session_id: SessionId, trace_id: TraceId) -> Self {
    Self {
      event_id: EventId::new(),
      session_id,
      turn_id: None,
      seq: None,
      timestamp_ms: now_millis(),
      model_epoch: None,
      model: None,
      tool_call_id: None,
      parent_event_id: None,
      trace_id,
      span_id: crate::ids::SpanId::new(),
    }
  }

  pub fn with_turn(mut self, turn_id: TurnId) -> Self {
    self.turn_id = Some(turn_id);
    self
  }

  pub fn with_epoch(mut self, epoch: u32, model: ModelRef) -> Self {
    self.model_epoch = Some(epoch);
    self.model = Some(model);
    self
  }

  pub fn with_tool_call(mut self, call_id: ToolCallId) -> Self {
    self.tool_call_id = Some(call_id);
    self
  }

  pub fn with_parent(mut self, parent: EventId) -> Self {
    self.parent_event_id = Some(parent);
    self
  }
}

/// Envelope written to disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventEnvelope {
  pub v: u32,
  pub meta: EventMeta,
  #[serde(flatten)]
  pub event: AgentEvent,
}

impl EventEnvelope {
  pub fn new(meta: EventMeta, event: AgentEvent) -> Self {
    Self {
      v: EVENT_SCHEMA_VERSION,
      meta,
      event,
    }
  }
}

/// Why a session ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionEndReason {
  /// The user asked to exit.
  UserExit,
  /// The runtime is restarting, for example after a config change.
  Restart,
  /// A fatal runtime condition ended the session.
  Fatal { message: String },
}

/// How a turn ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
  Completed,
  Cancelled,
  Failed { kind: ModelFailureKind },
}

/// Diagnostic severity for [`AgentEvent::Diagnostic`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticLevel {
  Info,
  Warn,
  Error,
}

/// The runtime event union.
///
/// Each variant documents why it exists, where it sits in the ordering,
/// whether it is persisted, what replay does with it, and why the UI cares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum AgentEvent {
  /// Why: the session exists and which model owns the first epoch.
  /// Ordering: first event of a session, before any user or model event.
  /// Persistence: always. Replay: creates the session shell and epoch 0.
  /// UI: establishes the header and model label.
  SessionStarted(SessionStarted),
  /// Why: a user message was accepted into canonical history.
  /// Ordering: after `session_started`, before the model request it triggers.
  /// Persistence: always. Replay: appends a user message.
  /// UI: renders the user turn.
  UserMessage(UserMessage),
  /// Why: a model request began, which is the boundary for partial output.
  /// Ordering: before any delta for that request.
  /// Persistence: always. Replay: opens a request span.
  /// UI: shows that work is in flight.
  ModelRequestStarted(ModelRequestStarted),
  /// Why: reasoning-like text arrived, with its provenance claim.
  /// Ordering: only between matching request start/complete events.
  /// Persistence: always in trace; not required in session state.
  /// Replay: appends a reasoning block that keeps its provenance.
  /// UI: renders under the provenance-specific style, never as plain prose.
  ReasoningDelta(ReasoningDelta),
  /// Why: assistant prose arrived.
  /// Ordering: only between matching request start/complete events.
  /// Persistence: always in trace; coalesced in session state.
  /// Replay: appends to the assistant message.
  /// UI: streams text.
  AssistantDelta(AssistantDelta),
  /// Why: a model request finished and is attributable.
  /// Ordering: closes the span opened by `model_request_started`.
  /// Persistence: always. Replay: closes the span and records usage.
  /// UI: replaces the in-flight indicator with final status.
  ModelRequestCompleted(ModelRequestCompleted),
  /// Why: the same request is being retried against the same model.
  /// Ordering: after a failure, before the next `model_request_started`.
  /// Persistence: always. Replay: annotates the attempt timeline.
  /// UI: rare, so it is emphasized in the status line.
  ModelRetry(ModelRetry),
  /// Why: availability failure moved generation to the backup model.
  /// Ordering: immediately before the `model_epoch_started` it caused.
  /// Persistence: always. Replay: opens a new epoch with this reason.
  /// UI: rare event, shown prominently with the cause.
  ModelFailover(ModelFailover),
  /// Why: which model owned a span of generation, and what it was believed
  /// capable of at that moment.
  /// Ordering: first for epoch 0; later epochs follow their switch/failover.
  /// Persistence: always. Replay: rebuilds the epoch list in order.
  /// UI: powers model attribution and the failover timeline.
  ModelEpochStarted(ModelEpochStarted),
  /// Why: the model asked for a tool call and arguments are fully decoded.
  /// Ordering: before `tool_started`; the call id never changes after this.
  /// Persistence: always. Replay: records a requested call.
  /// UI: shows the intended operation and arguments.
  ToolRequested(ToolRequested),
  /// Why: execution began, so a later crash has an observed boundary.
  /// Ordering: after `tool_requested`, before completion.
  /// Persistence: always. Replay: marks the call started.
  /// UI: shows the running operation.
  ToolStarted(ToolStarted),
  /// Why: the call completed with a committed result.
  /// Ordering: terminal for that call id.
  /// Persistence: always. Replay: attaches the result, or its reduction.
  /// UI: collapses output and marks reduction.
  ToolCompleted(ToolCompleted),
  /// Why: the call completed with an observed failure.
  /// Ordering: terminal for that call id.
  /// Persistence: always. Replay: attaches the failure.
  /// UI: shows the error and its cause.
  ToolFailed(ToolFailed),
  /// Why: completion could not be observed, which is not the same as failure.
  /// Ordering: terminal for that call id.
  /// Persistence: always. Replay: marks the call uncertain so no replay is
  /// attempted automatically.
  /// UI: emphasized; drives the reconcile path.
  ToolUnknown(ToolUnknown),
  /// Why: external knowledge entered context, with citation and provenance.
  /// Ordering: inside the turn that retrieved it.
  /// Persistence: always. Replay: reattaches the reference, not the payload.
  /// UI: renders the citation.
  ExternalContextRetrieved(ExternalContextRetrieved),
  /// Why: model-visible payload was reduced to a bounded representation.
  /// Ordering: after the payload it reduced exists in canonical history.
  /// Persistence: always. Replay: re-derives the model-visible form while
  /// keeping the full payload in the store.
  /// UI: shows that output was collapsed, with a recovery reference.
  ContextReduced(ContextReduced),
  /// Why: compaction began at a safe boundary.
  /// Ordering: at a turn or phase boundary, never mid-request.
  /// Persistence: always. Replay: applies the compaction marker.
  /// UI: rare, shows the level and reason.
  /// Producer: none found at 2026-09-07, test fixtures only
  /// (docs/COMPACT_EVENT_AUDIT.md).
  ContextCompactionStarted(ContextCompactionStarted),
  /// Why: compaction finished and what it retained.
  /// Ordering: closes a matching compaction start.
  /// Persistence: always. Replay: marks the context epoch advanced.
  /// UI: shows retained/removed counts.
  /// Producer: none found at 2026-09-07, test fixtures only
  /// (docs/COMPACT_EVENT_AUDIT.md).
  ContextCompactionCompleted(ContextCompactionCompleted),
  /// Why: a compaction moved the model-visible context to a new epoch, and this is
  /// the durable record of which canonical range it replaced and what stands in
  /// for that range.
  /// Ordering: never before the records it summarises. It is appended after the
  /// whole replaced range, and the log assigns `seq` monotonically at append time.
  /// Persistence: always. Replay: advances the context epoch and substitutes the
  /// summary for the replaced range in model-visible context only; canonical
  /// history is replayed unchanged.
  /// UI: rare, and it must read as "context changed", never as "history changed".
  ContextCompactionEpoch(ContextCompactionEpoch),
  /// Why: an episode checkpoint capsule was written.
  /// Ordering: after the events summarized by the capsule.
  /// Persistence: always. Replay: records the checkpoint pointer.
  /// UI: makes `/checkpoints` meaningful.
  CheckpointCreated(CheckpointCreated),
  /// Why: a turn ended and how.
  /// Ordering: last event of that turn.
  /// Persistence: always. Replay: closes the turn.
  /// UI: returns the editor to idle and reports the status.
  TurnCompleted(TurnCompleted),
  /// Why: an operator-visible condition that is not a domain event.
  /// Ordering: anywhere.
  /// Persistence: always; the only place free-form operator text is allowed,
  /// and only after redaction.
  /// Replay: shown as a diagnostic note, never as assistant content.
  /// UI: quiet by default, filtered in `/trace`.
  Diagnostic(Diagnostic),
  /// Why: the session ended.
  /// Ordering: last event of the session.
  /// Persistence: always. Replay: marks the session closed.
  /// UI: exits or reports closure.
  SessionEnded(SessionEnded),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStarted {
  pub working_dir: String,
  pub model: ModelRef,
  pub capabilities: ModelCapabilities,
  /// `true` when the session continues an existing journal.
  #[serde(default)]
  pub resumed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserMessage {
  pub text: String,
  /// Number of non-text blocks, for example images. The payload itself lives
  /// in session state and trace blobs.
  #[serde(default)]
  pub attachments: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRequestStarted {
  pub epoch: u32,
  pub model: ModelRef,
  pub message_count: u32,
  /// Estimate, not a measurement. Recorded so that context decisions are
  /// auditable later.
  pub context_tokens_est: u64,
  pub tools_exposed: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningDelta {
  pub text: String,
  pub provenance: ReasoningProvenance,
  pub chunk_index: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantDelta {
  pub text: String,
  pub chunk_index: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRequestCompleted {
  pub epoch: u32,
  pub model: ModelRef,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub finish_reason: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub input_tokens: Option<u64>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub output_tokens: Option<u64>,
  pub duration_ms: u64,
  pub tool_calls: u32,
  /// Provenance of reasoning seen during this request, `None` when none was
  /// exposed. Recorded so that provenance survives even if reasoning deltas
  /// were coalesced or reduced.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub reasoning_provenance: Option<ReasoningProvenance>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRetry {
  pub attempt: u32,
  pub max_attempts: u32,
  pub kind: ModelFailureKind,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub retry_after_ms: Option<u64>,
  /// `true` when the next action is takeover rather than another retry.
  #[serde(default)]
  pub will_failover: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelFailover {
  pub from: ModelRef,
  pub to: ModelRef,
  pub kind: ModelFailureKind,
  /// Gaps that had to be tolerated or repaired for takeover.
  #[serde(default)]
  pub gaps: Vec<CapabilityGap>,
  /// `true` when the context was rebudgeted before takeover.
  #[serde(default)]
  pub compacted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelEpochStarted {
  pub epoch: u32,
  pub model: ModelRef,
  pub reason: EpochReason,
  pub capabilities: ModelCapabilities,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRequested {
  pub call_id: ToolCallId,
  pub name: String,
  pub arguments: serde_json::Value,
  #[serde(default)]
  pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolStarted {
  pub call_id: ToolCallId,
  pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCompleted {
  pub call_id: ToolCallId,
  pub name: String,
  /// Always [`ToolExecutionState::Succeeded`]; kept explicit so that the
  /// journal states the claim instead of implying it.
  pub state: ToolExecutionState,
  pub duration_ms: u64,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub status: Option<i64>,
  /// `true` when the model-visible form is a reduction of a larger payload.
  #[serde(default)]
  pub reduced: bool,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub blob: Option<BlobRef>,
  /// Model-visible text length after reduction, so that size claims stay
  /// checkable.
  pub visible_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolFailed {
  pub call_id: ToolCallId,
  pub name: String,
  pub message: String,
  pub duration_ms: u64,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub status: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolUnknown {
  pub call_id: ToolCallId,
  pub name: String,
  /// What is unknown, written for a human reader: the boundary that was not
  /// observed.
  pub why: String,
  /// `true` when the call could have changed external state.
  pub mutating: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalContextRetrieved {
  pub source: ExternalContextSource,
  pub citation: Option<String>,
  pub bytes: u64,
  /// `true` when the payload entered context inline rather than as a
  /// reference.
  pub inline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextReduced {
  pub reason: ReductionReason,
  pub original_bytes: u64,
  pub visible_bytes: u64,
  /// Where the withheld bytes went, when somewhere could hold them.
  ///
  /// `None` means the bytes are gone and only this record remains. The event is
  /// still emitted, because "the model was shown less" is the fact that has to
  /// survive; whether it can be undone is a second question, and a record that only
  /// existed when recovery was possible would silently delete itself in exactly the
  /// case that needs auditing.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub blob: Option<BlobRef>,
  /// Human-usable pointer back to the full payload, for example
  /// `blobs/7f2c…`. Recovery must be possible from this string alone, and its
  /// absence says plainly that there is nothing to recover from.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub recovery_ref: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub tool_call_id: Option<ToolCallId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCompactionStarted {
  pub level: ContextLevel,
  pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCompactionCompleted {
  pub level: ContextLevel,
  pub removed_messages: u32,
  pub retained_messages: u32,
  pub context_epoch: u32,
}

/// The first compaction epoch. Epoch `0` is the uncompacted context, where every
/// canonical record is also in the model-visible context, so the first record of
/// a compaction claims `1`.
pub const FIRST_COMPACTION_EPOCH: u32 = 1;

/// The durable record of one context compaction epoch.
///
/// This is a record that a compaction happened, not compaction itself: it carries
/// no summary body and rewrites nothing.
///
/// Canonical history is **never** rewritten. The record is appended, and every
/// record inside `replaces_from..=replaces_through` stays readable through
/// `pi-rs trace`; only the model-visible context swaps that range for `summary`.
/// Holding an opaque [`BlobRef`] instead of the summary text is what keeps a
/// reduced context from being mistaken for the canonical trace.
///
/// The session is named by [`EventMeta::session_id`], as in every other record,
/// so this struct carries only what a compaction adds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCompactionEpoch {
  /// Monotonic epoch ordinal starting at [`FIRST_COMPACTION_EPOCH`]. Claim it with
  /// [`next_context_epoch`] rather than from an in-memory counter, so a resume
  /// numbers epochs exactly as the writer did. Spelled as in
  /// [`ContextCompactionCompleted`], because a bare `epoch` reads as the model epoch.
  pub context_epoch: u32,
  /// First canonical sequence number the summary replaces in model-visible context.
  /// Inclusive, and the same coordinate [`EventMeta::seq`] uses — no second
  /// addressing system exists.
  pub replaces_from: EventSeq,
  /// Last canonical sequence number the summary replaces. Inclusive, and never
  /// below `replaces_from`: a summary always stands in for at least one record.
  pub replaces_through: EventSeq,
  /// Reference to the stored summary, never the summary itself.
  pub summary: BlobRef,
}

/// The epoch ordinal the next [`ContextCompactionEpoch`] record must claim.
///
/// Derived from the records themselves, never from an in-memory counter: a process
/// that resumes a session reads its trace and must compute the same numbering as
/// the process that wrote it. Ordinals are dense, so the next one is one past the
/// highest recorded, and a trace with no compaction is still at the first.
pub fn next_context_epoch<'events>(events: impl IntoIterator<Item = &'events AgentEvent>) -> u32 {
  let highest = events
    .into_iter()
    .fold(None::<u32>, |highest, event| match event {
      AgentEvent::ContextCompactionEpoch(record) => {
        Some(highest.map_or(record.context_epoch, |high| high.max(record.context_epoch)))
      }
      _ => highest,
    });
  highest.map_or(FIRST_COMPACTION_EPOCH, |epoch| epoch + 1)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointCreated {
  pub checkpoint_id: CheckpointId,
  pub capsule_version: u32,
  pub summarized_events: u64,
  pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnCompleted {
  pub status: TurnStatus,
  pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
  pub level: DiagnosticLevel,
  pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEnded {
  pub reason: SessionEndReason,
}

/// A message paired with the event that introduced it, used by session
/// reconstruction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributedMessage {
  pub envelope: EventEnvelope,
  pub message: Message,
}

#[cfg(test)]
mod tests {
  use crate::context::ContextLevel;

  use super::*;

  fn meta() -> EventMeta {
    EventMeta::new(SessionId::new(), TraceId::new())
  }

  fn model() -> ModelRef {
    ModelRef::new("local", "qwen")
  }

  /// A reduction record must read both ways.
  ///
  /// Older journals always carried a blob and a reference; a new one may carry
  /// neither, when the bytes were withheld and nowhere could hold them. Requiring
  /// either field would make an existing session unreadable, and emitting two nulls
  /// on every ordinary reduction would tax every line for a fact that is usually
  /// absent.
  #[test]
  fn a_reduction_record_reads_both_ways() {
    let reduced = ContextReduced {
      reason: ReductionReason::RecentTargetExceeded { target_tokens: 64 },
      original_bytes: 12_000,
      visible_bytes: 300,
      blob: Some(BlobRef::for_bytes(b"payload".as_slice(), None)),
      recovery_ref: Some("blobs/aa:aa".into()),
      tool_call_id: None,
    };
    let recorded = serde_json::to_value(&reduced).unwrap();
    assert_eq!(recorded["recovery_ref"], "blobs/aa:aa", "{recorded}");
    let decoded: ContextReduced = serde_json::from_value(recorded).unwrap();
    assert_eq!(decoded, reduced, "a line that had both still reads");

    let unrecoverable = ContextReduced {
      blob: None,
      recovery_ref: None,
      ..reduced
    };
    let stored = serde_json::to_value(&unrecoverable).unwrap();
    assert!(stored.get("blob").is_none(), "{stored}");
    assert!(stored.get("recovery_ref").is_none(), "{stored}");
    let decoded: ContextReduced = serde_json::from_value(stored).unwrap();
    assert_eq!(decoded.recovery_ref, None, "absence decodes as absence");
  }

  #[test]
  fn envelope_is_versioned_and_tagged() {
    let envelope = EventEnvelope::new(
      meta(),
      AgentEvent::SessionStarted(SessionStarted {
        working_dir: "/repo".into(),
        model: model(),
        capabilities: ModelCapabilities::text_only(32_000),
        resumed: false,
      }),
    );
    let encoded = serde_json::to_string(&envelope).unwrap();
    assert!(encoded.contains("\"v\":1"), "{encoded}");
    assert!(
      encoded.contains("\"type\":\"session_started\""),
      "{encoded}"
    );
    let decoded: EventEnvelope = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, envelope);
  }

  #[test]
  fn reasoning_delta_keeps_provenance_in_envelope() {
    let envelope = EventEnvelope::new(
      meta(),
      AgentEvent::ReasoningDelta(ReasoningDelta {
        text: "checking the test".into(),
        provenance: ReasoningProvenance::Native,
        chunk_index: 0,
      }),
    );
    let encoded = serde_json::to_string(&envelope).unwrap();
    let decoded: EventEnvelope = serde_json::from_str(&encoded).unwrap();
    let AgentEvent::ReasoningDelta(delta) = decoded.event else {
      panic!("expected reasoning delta, got {:?}", decoded.event);
    };
    assert_eq!(delta.provenance, ReasoningProvenance::Native);
  }

  #[test]
  fn tool_completion_claims_state_explicitly() {
    let call_id = ToolCallId::new();
    let envelope = EventEnvelope::new(
      meta().with_tool_call(call_id.clone()),
      AgentEvent::ToolCompleted(ToolCompleted {
        call_id,
        name: "read".into(),
        state: ToolExecutionState::Succeeded,
        duration_ms: 12,
        status: None,
        reduced: false,
        blob: None,
        visible_bytes: 220,
      }),
    );
    let encoded = serde_json::to_string(&envelope).unwrap();
    assert!(encoded.contains("\"state\":\"succeeded\""), "{encoded}");
    assert!(
      encoded.contains("\"tool_call_id\""),
      "tool completion must be attributable to one call: {encoded}"
    );
  }

  #[test]
  fn ordering_key_is_optional_until_the_log_assigns_it() {
    let mut envelope = EventEnvelope::new(
      meta(),
      AgentEvent::Diagnostic(Diagnostic {
        level: DiagnosticLevel::Info,
        message: "loaded 3 skills".into(),
      }),
    );
    assert_eq!(envelope.meta.seq, None);
    envelope.meta.seq = Some(EventSeq(1));
    let encoded = serde_json::to_string(&envelope).unwrap();
    assert!(encoded.contains("\"seq\":1"), "{encoded}");
  }

  #[test]
  fn unknown_tool_completion_is_not_failure() {
    let event = AgentEvent::ToolUnknown(ToolUnknown {
      call_id: ToolCallId::new(),
      name: "exec".into(),
      why: "process exited before status was read".into(),
      mutating: true,
    });
    let encoded = serde_json::to_string(&event).unwrap();
    assert!(encoded.contains("\"type\":\"tool_unknown\""), "{encoded}");
    assert!(encoded.contains("\"mutating\":true"), "{encoded}");
  }

  fn epoch_record(epoch: u32, from: u64, through: u64) -> ContextCompactionEpoch {
    ContextCompactionEpoch {
      context_epoch: epoch,
      replaces_from: EventSeq(from),
      replaces_through: EventSeq(through),
      summary: BlobRef::for_bytes(b"summary text", Some("text/plain")),
    }
  }

  #[test]
  fn compaction_epoch_round_trips_through_event_serialization() {
    let event = AgentEvent::ContextCompactionEpoch(epoch_record(1, 4, 12));
    let encoded = serde_json::to_string(&event).unwrap();
    assert!(
      encoded.contains("\"type\":\"context_compaction_epoch\""),
      "{encoded}"
    );
    // The range is the existing sequence coordinate, and the summary is a
    // reference: the summary body must never be serialized into the record.
    assert!(encoded.contains("\"replaces_from\":4"), "{encoded}");
    assert!(encoded.contains("\"replaces_through\":12"), "{encoded}");
    assert!(!encoded.contains("summary text"), "{encoded}");
    let decoded: AgentEvent = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, event);
  }

  #[test]
  fn compaction_epoch_ordinals_are_derived_from_the_records() {
    // No compaction yet: the context is still at the first epoch ordinal.
    assert_eq!(next_context_epoch([]), FIRST_COMPACTION_EPOCH);
    let records: Vec<AgentEvent> = (FIRST_COMPACTION_EPOCH..=3)
      .map(|epoch| {
        AgentEvent::ContextCompactionEpoch(epoch_record(epoch, epoch as u64, epoch as u64 + 1))
      })
      .collect();
    let mut events: Vec<AgentEvent> = records
      .iter()
      .map(|_| {
        AgentEvent::UserMessage(UserMessage {
          text: "noise".into(),
          attachments: 0,
        })
      })
      .chain(records.clone())
      .collect();
    assert_eq!(next_context_epoch(events.iter()), 4);
    // Derivation reads the ordinals, not the order the records arrived in.
    events.reverse();
    assert_eq!(next_context_epoch(events.iter()), 4);
  }

  #[test]
  fn compaction_events_carry_level() {
    let event = AgentEvent::ContextCompactionStarted(ContextCompactionStarted {
      level: ContextLevel::L1Ordinary,
      reason: "recent-context target exceeded".into(),
    });
    let encoded = serde_json::to_string(&event).unwrap();
    assert!(encoded.contains("l1_ordinary"), "{encoded}");
    let decoded: AgentEvent = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, event);
  }

  /// Why: neither compaction variant has a production producer (see
  /// docs/COMPACT_EVENT_AUDIT.md), so the serialized shape is the only
  /// contract that keeps it compatible. Pin the `type` tag and every field
  /// name exactly, so a rename or a dropped field fails here.
  #[test]
  fn compaction_started_wire_shape_is_pinned() {
    let event = AgentEvent::ContextCompactionStarted(ContextCompactionStarted {
      level: ContextLevel::L1Ordinary,
      reason: "recent-context target exceeded".into(),
    });
    let encoded = serde_json::to_string(&event).unwrap();
    assert_eq!(
      encoded,
      "{\"type\":\"context_compaction_started\",\"level\":\"l1_ordinary\",\"reason\":\"recent-context target exceeded\"}"
    );
    let decoded: AgentEvent = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, event);
  }

  /// Why: as for the start variant. Also pins that the counts and
  /// `context_epoch` stay numbers on the wire, never strings.
  #[test]
  fn compaction_completed_wire_shape_is_pinned() {
    let event = AgentEvent::ContextCompactionCompleted(ContextCompactionCompleted {
      level: ContextLevel::L2Phase,
      removed_messages: 12,
      retained_messages: 30,
      context_epoch: 2,
    });
    let encoded = serde_json::to_string(&event).unwrap();
    assert_eq!(
      encoded,
      "{\"type\":\"context_compaction_completed\",\"level\":\"l2_phase\",\"removed_messages\":12,\"retained_messages\":30,\"context_epoch\":2}"
    );
    let decoded: AgentEvent = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, event);
  }
}
