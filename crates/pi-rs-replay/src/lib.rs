//! Pure replay and research-analysis helpers for recorded `pi-rs` sessions.
//!
//! This crate deliberately has no store, tool, provider, CLI, or TUI dependency. It consumes
//! already-persisted [`TraceEntry`] values and optional [`SessionRecord`] values, then derives
//! deterministic views: filtered replay slices, historical references, model-visible context,
//! dry branch plans, structural continuation comparisons, timelines, provenance summaries, and
//! redacted export values. Historical facts stay separate from any future continuation; nothing in
//! this crate executes a provider request or a tool call.

use std::collections::{BTreeMap, BTreeSet};

use pi_rs_core::{
  AgentEvent, BlobRef, CapabilityGap, CheckpointId, ContextCapsule, ContextLevel, EpochReason,
  EventId, EventSeq, ExternalContextRef, ExternalizedField, Message, ModelCapabilities, ModelRef,
  ReasoningProvenance, Role, SessionMessage, SessionRecord, ToolCallId, ToolExecutionState,
  TraceEntry,
};
use serde::{Deserialize, Serialize};

/// Selects replay records by their canonical event address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum HistoricalTarget {
  /// Stop or branch at the event with this durable event id.
  EventId(EventId),
  /// Stop or branch at the event carrying this per-session sequence.
  Seq(EventSeq),
}

impl HistoricalTarget {
  fn matches(&self, entry: &TraceEntry) -> bool {
    match self {
      Self::EventId(id) => &entry.envelope.meta.event_id == id,
      Self::Seq(seq) => entry.envelope.meta.seq == Some(*seq),
    }
  }
}

/// Durable address of one historical event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoricalEventRef {
  pub session_id: pi_rs_core::SessionId,
  pub event_id: EventId,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub seq: Option<EventSeq>,
}

impl HistoricalEventRef {
  pub fn from_entry(entry: &TraceEntry) -> Self {
    Self {
      session_id: entry.envelope.meta.session_id.clone(),
      event_id: entry.envelope.meta.event_id.clone(),
      seq: entry.envelope.meta.seq,
    }
  }

  pub fn target(&self) -> HistoricalTarget {
    self
      .seq
      .map(HistoricalTarget::Seq)
      .unwrap_or_else(|| HistoricalTarget::EventId(self.event_id.clone()))
  }
}

/// Durable address of a session message and its introducing event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoricalMessageRef {
  /// Index of this non-header session line in `session.jsonl`.
  pub session_line: u32,
  pub event_id: EventId,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub seq: Option<EventSeq>,
}

/// User-selected replay filters. If all fields are false, every event is selected.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayFilters {
  /// Select tool lifecycle events.
  pub tools: bool,
  /// Select reasoning deltas and model-epoch/failover attribution boundaries.
  pub reasoning: bool,
  /// Select events that carry timestamp, duration, retry-after, or timeline timing facts.
  pub timing: bool,
}

impl ReplayFilters {
  pub fn all() -> Self {
    Self::default()
  }

  pub fn tools() -> Self {
    Self {
      tools: true,
      ..Self::default()
    }
  }

  pub fn reasoning() -> Self {
    Self {
      reasoning: true,
      ..Self::default()
    }
  }

  pub fn timing() -> Self {
    Self {
      timing: true,
      ..Self::default()
    }
  }

  fn selects(&self, event: &AgentEvent) -> bool {
    if !self.tools && !self.reasoning && !self.timing {
      return true;
    }
    (self.tools && is_tool_event(event))
      || (self.reasoning && is_reasoning_event(event))
      || (self.timing && is_timing_event(event))
  }
}

/// Options for a deterministic replay projection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayOptions {
  pub filters: ReplayFilters,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub until: Option<HistoricalTarget>,
}

/// A replayed event plus derived facts. The event is historical data, never an instruction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayedEvent {
  pub reference: HistoricalEventRef,
  pub kind: EventKind,
  pub event: AgentEvent,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub timing: Option<TimingFact>,
  #[serde(default, skip_serializing_if = "is_zero")]
  pub redactions: u32,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub externalized: Vec<ExternalizedField>,
  /// Whether a raw payload existed in the persisted trace. The payload itself is not replayed.
  #[serde(default)]
  pub raw_payload_attached: bool,
}

/// Coarse stable event kind for replay filtering and continuation comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
  SessionStarted,
  UserMessage,
  ModelRequestStarted,
  ReasoningDelta,
  AssistantDelta,
  ModelRequestCompleted,
  ModelRetry,
  ModelFailover,
  ModelEpochStarted,
  ToolRequested,
  ToolStarted,
  ToolCompleted,
  ToolFailed,
  ToolUnknown,
  ExternalContextRetrieved,
  ContextReduced,
  ContextCompactionStarted,
  ContextCompactionCompleted,
  ContextSummary,
  ContextCompactionEpoch,
  CheckpointCreated,
  TurnCompleted,
  Diagnostic,
  SessionEnded,
}

/// Timing-related replay data. Sequence order still defines causality; timestamps are human facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimingFact {
  pub timestamp_ms: u64,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub duration_ms: Option<u64>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub retry_after_ms: Option<u64>,
}

/// Deterministic replay projection and common derived analyses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayReport {
  pub events: Vec<ReplayedEvent>,
  pub tool_states: Vec<ToolReplayState>,
  pub model_epochs: Vec<ModelEpochTimelineEntry>,
  pub compactions: Vec<CompactionTimelineEntry>,
  pub failovers: Vec<FailoverTimelineEntry>,
  pub provenance: ProvenanceSummary,
}

/// Errors returned by pure replay projections.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ReplayError {
  HistoricalTargetNotFound { target: HistoricalTarget },
}

impl std::fmt::Display for ReplayError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::HistoricalTargetNotFound { target } => {
        write!(f, "historical target not found: {target:?}")
      }
    }
  }
}

impl std::error::Error for ReplayError {}

/// Replay a trace in canonical sequence order, optionally stopping at an event inclusively.
pub fn replay_trace(
  entries: &[TraceEntry],
  options: &ReplayOptions,
) -> Result<ReplayReport, ReplayError> {
  let ordered = ordered_entries(entries);
  let bounded = apply_until(ordered, &options.until)?;
  let replayed = bounded
    .iter()
    .filter(|entry| options.filters.selects(&entry.envelope.event))
    .map(|entry| ReplayedEvent {
      reference: HistoricalEventRef::from_entry(entry),
      kind: EventKind::of(&entry.envelope.event),
      event: entry.envelope.event.clone(),
      timing: timing_fact(entry),
      redactions: entry.redactions,
      externalized: entry.externalized.clone(),
      raw_payload_attached: entry.raw_payload,
    })
    .collect();
  Ok(ReplayReport {
    events: replayed,
    tool_states: tool_replay_states_from_entries(&bounded),
    model_epochs: model_epoch_timeline_from_entries(&bounded),
    compactions: compaction_timeline_from_entries(&bounded),
    failovers: failover_timeline_from_entries(&bounded),
    provenance: provenance_summary(&bounded, &[]),
  })
}

/// Replay a trace while joining the bounded session projection for provenance claims.
///
/// Session messages are filtered by their introducing sequence before the summary is built, so
/// `--until` cannot accidentally report reasoning that happened after the selected event.
pub fn replay_trace_with_session(
  entries: &[TraceEntry],
  session_records: &[SessionRecord],
  options: &ReplayOptions,
) -> Result<ReplayReport, ReplayError> {
  let mut report = replay_trace(entries, options)?;
  let ordered = ordered_entries(entries);
  let bounded = apply_until(ordered, &options.until)?;
  let bounded_session = session_records_until(session_records, options.until.as_ref(), &bounded);
  report.provenance = provenance_summary(&bounded, &bounded_session);
  Ok(report)
}

/// Reconstruct canonical and model-visible context from persisted session state and trace epochs.
pub fn reconstruct_context(
  session_records: &[SessionRecord],
  trace_entries: &[TraceEntry],
) -> ContextReconstruction {
  let mut canonical = Vec::new();
  let mut checkpoints: Vec<(u32, SessionCheckpointRecordView)> = Vec::new();
  let mut compaction_records: Vec<(u32, pi_rs_core::SessionCompactionRecord)> = Vec::new();
  let mut session_line = 0u32;

  for record in session_records {
    match record {
      SessionRecord::Header(_) => {}
      SessionRecord::Message(message) => {
        canonical.push(HistoricalMessage::from_session_message(
          session_line,
          message,
        ));
        session_line = session_line.saturating_add(1);
      }
      SessionRecord::CheckpointBarrier(checkpoint) => {
        checkpoints.push((session_line, SessionCheckpointRecordView::from(checkpoint)));
        session_line = session_line.saturating_add(1);
      }
      SessionRecord::Compaction(compaction) => {
        compaction_records.push((session_line, compaction.clone()));
        session_line = session_line.saturating_add(1);
      }
      SessionRecord::Epoch(_) => {
        session_line = session_line.saturating_add(1);
      }
    }
  }

  let latest_checkpoint = checkpoints.last().cloned();
  let mut working = Vec::new();
  if let Some((barrier_line, checkpoint)) = latest_checkpoint.clone() {
    working.push(WorkingContextItem::CheckpointCapsule {
      checkpoint_id: checkpoint.checkpoint_id,
      capsule_version: checkpoint.capsule_version,
      capsule_text: checkpoint.capsule.format_for_model(),
      capsule: checkpoint.capsule,
    });
    working.extend(
      canonical
        .iter()
        .filter(|message| message.reference.session_line > barrier_line)
        .cloned()
        .map(WorkingContextItem::Message),
    );
  } else {
    working.extend(canonical.iter().cloned().map(WorkingContextItem::Message));
  }

  let epoch_records = context_epoch_records(trace_entries);
  if compaction_records
    .iter()
    .any(|(_, compaction)| compaction.summary_present)
  {
    // New session projections carry the exact retained-tail count. Prefer them
    // over the trace range here: an epoch range intentionally names canonical
    // history, while a runtime compaction may retain messages inside that range.
    working = projected_working_context(session_records, &canonical);
  } else if epoch_records.is_empty() {
    if let Some((_, compaction)) = compaction_records.last() {
      working.retain(|item| match item {
        WorkingContextItem::Message(message) => {
          message.reference.session_line >= compaction.retained_from
        }
        _ => true,
      });
    }
  } else {
    for epoch in &epoch_records {
      apply_context_epoch(&mut working, epoch);
    }
  }

  let context_epoch = epoch_records
    .last()
    .map(|epoch| epoch.context_epoch)
    .or_else(|| compaction_records.last().map(|(_, c)| c.context_epoch))
    .unwrap_or(0);

  ContextReconstruction {
    canonical: CanonicalContext {
      messages: canonical,
    },
    working: WorkingContext {
      context_epoch,
      items: working,
    },
    compaction_epochs: epoch_records,
  }
}

/// Rebuild the model-visible projection from semantic session records.
///
/// This path is used when a current writer projected `summary_present`: the marker's
/// retained count is authoritative for the live window, and the canonical trace is
/// intentionally not asked to infer which retained messages shared an event range.
fn projected_working_context(
  records: &[SessionRecord],
  canonical: &[HistoricalMessage],
) -> Vec<WorkingContextItem> {
  let mut working = Vec::new();
  let mut canonical_index = 0usize;
  for record in records {
    match record {
      SessionRecord::Message(_) => {
        if let Some(message) = canonical.get(canonical_index) {
          working.push(WorkingContextItem::Message(message.clone()));
        }
        canonical_index = canonical_index.saturating_add(1);
      }
      SessionRecord::CheckpointBarrier(checkpoint) => {
        working.clear();
        working.push(WorkingContextItem::CheckpointCapsule {
          checkpoint_id: checkpoint.checkpoint_id.clone(),
          capsule_version: checkpoint.capsule_version,
          capsule_text: checkpoint.capsule.format_for_model(),
          capsule: checkpoint.capsule.clone(),
        });
      }
      SessionRecord::Compaction(compaction)
        if compaction.summary_present
          && matches!(
            compaction.level,
            pi_rs_core::ContextLevel::L1Ordinary | pi_rs_core::ContextLevel::L2Phase
          ) =>
      {
        let Some(summary_index) = working
          .iter()
          .rposition(|item| matches!(item, WorkingContextItem::Message(_)))
        else {
          continue;
        };
        let summary = working.remove(summary_index);
        let floor = working
          .iter()
          .take_while(|item| matches!(item, WorkingContextItem::CheckpointCapsule { .. }))
          .count();
        let retained = compaction.retained_messages as usize;
        let tail_start = working.len().saturating_sub(retained);
        let tail = working.split_off(tail_start);
        working.truncate(floor);
        working.push(summary);
        working.extend(tail);
      }
      _ => {}
    }
  }
  working
}

/// Construct a dry, pure plan for branching from a historical event.
pub fn plan_historical_branch(
  trace_entries: &[TraceEntry],
  session_records: &[SessionRecord],
  target: HistoricalTarget,
) -> Result<HistoricalBranchPlan, ReplayError> {
  let ordered = ordered_entries(trace_entries);
  let bounded = apply_until(ordered, &Some(target.clone()))?;
  let branch_point = HistoricalEventRef::from_entry(
    bounded
      .last()
      .expect("apply_until with a found target returns at least the target"),
  );
  let bounded_session = session_records_until(session_records, Some(&target), &bounded);
  let bounded_owned: Vec<TraceEntry> = bounded.iter().map(|entry| (*entry).clone()).collect();
  let context = reconstruct_context(&bounded_session, &bounded_owned);
  let blocked_tools = tool_replay_states_from_entries(&bounded)
    .into_iter()
    .filter(|state| {
      matches!(
        state.decision,
        HistoricalToolDecision::ReconcileBeforeReplay
      )
    })
    .collect();

  Ok(HistoricalBranchPlan {
    branch_point,
    historical_events: bounded
      .into_iter()
      .map(HistoricalEventRef::from_entry)
      .collect(),
    context,
    blocked_tools,
    new_execution: NewExecutionBoundary {
      planned_only: true,
      history_separated: true,
      note: "dry branch plan only; no provider or tool execution performed".to_string(),
    },
  })
}

/// Compare two possible continuations structurally without judging output quality.
///
/// Each input is normally a full trace containing `base`; when its event id is absent, the input
/// is treated as an already-sliced continuation. Sequence numbers can restart on a branch, so
/// they are never used to discard such records implicitly.
pub fn compare_continuations(
  base: HistoricalEventRef,
  left: &[TraceEntry],
  right: &[TraceEntry],
) -> ContinuationComparison {
  let left_signature = structural_signature(&continuation_after(&base, left));
  let right_signature = structural_signature(&continuation_after(&base, right));
  let max = left_signature.len().max(right_signature.len());
  let diverged_at = (0..max).find(|&index| left_signature.get(index) != right_signature.get(index));
  ContinuationComparison {
    base,
    structurally_equal: diverged_at.is_none(),
    diverged_at,
    left_len: left_signature.len(),
    right_len: right_signature.len(),
    left_at_divergence: diverged_at.and_then(|index| left_signature.get(index).cloned()),
    right_at_divergence: diverged_at.and_then(|index| right_signature.get(index).cloned()),
    left_signature,
    right_signature,
  }
}

/// Build a model epoch timeline in canonical order.
pub fn model_epoch_timeline(entries: &[TraceEntry]) -> Vec<ModelEpochTimelineEntry> {
  model_epoch_timeline_from_entries(&ordered_entries(entries))
}

/// Build a context compaction/checkpoint timeline in canonical order.
pub fn compaction_timeline(entries: &[TraceEntry]) -> Vec<CompactionTimelineEntry> {
  compaction_timeline_from_entries(&ordered_entries(entries))
}

/// Build a failover timeline in canonical order.
pub fn failover_timeline(entries: &[TraceEntry]) -> Vec<FailoverTimelineEntry> {
  failover_timeline_from_entries(&ordered_entries(entries))
}

/// Summarize provenance claims that are actually present in trace and session state.
pub fn summarize_provenance(
  trace_entries: &[TraceEntry],
  session_records: &[SessionRecord],
) -> ProvenanceSummary {
  provenance_summary(&ordered_entries(trace_entries), session_records)
}

/// Export already-redacted normalized trace values. Raw payload pointers are intentionally omitted.
pub fn export_redacted_trace(
  entries: &[TraceEntry],
  filters: &ReplayFilters,
) -> Vec<RedactedTraceExportEntry> {
  ordered_entries(entries)
    .into_iter()
    .filter(|entry| filters.selects(&entry.envelope.event))
    .map(|entry| {
      let mut value = serde_json::to_value(&entry.envelope)
        .expect("event envelope serialization is part of pi-rs-core contract");
      if let serde_json::Value::Object(map) = &mut value {
        if entry.redactions > 0 {
          map.insert(
            "redactions".to_string(),
            serde_json::json!(entry.redactions),
          );
        }
        if entry.raw_payload {
          map.insert("raw_payload_attached".to_string(), serde_json::json!(true));
        }
        if !entry.externalized.is_empty() {
          map.insert(
            "externalized".to_string(),
            serde_json::to_value(&entry.externalized)
              .expect("externalized field serialization is infallible"),
          );
        }
      }
      RedactedTraceExportEntry {
        reference: HistoricalEventRef::from_entry(entry),
        kind: EventKind::of(&entry.envelope.event),
        value,
      }
    })
    .collect()
}

/// Result of model-visible context reconstruction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextReconstruction {
  pub canonical: CanonicalContext,
  pub working: WorkingContext,
  pub compaction_epochs: Vec<ContextEpochRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalContext {
  pub messages: Vec<HistoricalMessage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkingContext {
  pub context_epoch: u32,
  pub items: Vec<WorkingContextItem>,
}

/// One canonical session message with historical attribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoricalMessage {
  pub reference: HistoricalMessageRef,
  pub turn_id: pi_rs_core::TurnId,
  pub role: Role,
  pub message: Message,
  pub epoch: u32,
  pub model: ModelRef,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub external_context: Option<ExternalContextRef>,
}

impl HistoricalMessage {
  fn from_session_message(session_line: u32, message: &SessionMessage) -> Self {
    Self {
      reference: HistoricalMessageRef {
        session_line,
        event_id: message.event_id.clone(),
        seq: message.seq,
      },
      turn_id: message.turn_id.clone(),
      role: message.role,
      message: message.message.clone(),
      epoch: message.epoch,
      model: message.model.clone(),
      external_context: message.external_context.clone(),
    }
  }
}

/// One item in reconstructed model-visible context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum WorkingContextItem {
  Message(HistoricalMessage),
  CheckpointCapsule {
    checkpoint_id: CheckpointId,
    capsule_version: u32,
    capsule_text: String,
    capsule: ContextCapsule,
  },
  CompactionSummary {
    context_epoch: u32,
    replaces_from: EventSeq,
    replaces_through: EventSeq,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary: Option<BlobRef>,
  },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionCheckpointRecordView {
  checkpoint_id: CheckpointId,
  capsule_version: u32,
  capsule: ContextCapsule,
}

impl From<&pi_rs_core::SessionCheckpointRecord> for SessionCheckpointRecordView {
  fn from(record: &pi_rs_core::SessionCheckpointRecord) -> Self {
    Self {
      checkpoint_id: record.checkpoint_id.clone(),
      capsule_version: record.capsule_version,
      capsule: record.capsule.clone(),
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextEpochRef {
  pub reference: HistoricalEventRef,
  pub context_epoch: u32,
  pub replaces_from: EventSeq,
  pub replaces_through: EventSeq,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub summary: Option<BlobRef>,
}

/// Dry branch plan; `new_execution` is an explicit boundary rather than work to perform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoricalBranchPlan {
  pub branch_point: HistoricalEventRef,
  pub historical_events: Vec<HistoricalEventRef>,
  pub context: ContextReconstruction,
  pub blocked_tools: Vec<ToolReplayState>,
  pub new_execution: NewExecutionBoundary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewExecutionBoundary {
  pub planned_only: bool,
  pub history_separated: bool,
  pub note: String,
}

/// Last observed lifecycle state for one historical tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolReplayState {
  pub call_id: ToolCallId,
  pub name: String,
  pub state: ToolExecutionState,
  pub read_only: bool,
  pub mutating_unknown: bool,
  pub reference: HistoricalEventRef,
  pub decision: HistoricalToolDecision,
}

/// What replay/branch logic may do with a historical tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoricalToolDecision {
  /// A successful or failed terminal result is already canonical history.
  ReuseCommittedResult,
  /// Read-only uncommitted/unknown work may be rerun by a future runtime policy.
  SafeToReplayReadOnly,
  /// Mutating uncertain work must be reconciled before any repetition.
  ReconcileBeforeReplay,
  /// A request exists but no execution boundary was observed.
  RequestedOnly,
}

/// Timeline item for a model epoch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelEpochTimelineEntry {
  pub reference: HistoricalEventRef,
  pub epoch: u32,
  pub model: ModelRef,
  pub reason: EpochReason,
  pub capabilities: ModelCapabilities,
}

/// Timeline item for compaction and checkpoint events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CompactionTimelineEntry {
  Started {
    reference: HistoricalEventRef,
    level: ContextLevel,
    reason: String,
  },
  Completed {
    reference: HistoricalEventRef,
    level: ContextLevel,
    removed_messages: u32,
    retained_messages: u32,
    context_epoch: u32,
  },
  Epoch {
    reference: HistoricalEventRef,
    context_epoch: u32,
    replaces_from: EventSeq,
    replaces_through: EventSeq,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary: Option<BlobRef>,
  },
  Checkpoint {
    reference: HistoricalEventRef,
    checkpoint_id: CheckpointId,
    capsule_version: u32,
    summarized_events: u64,
    path: String,
  },
}

/// Timeline item for availability-driven failover.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailoverTimelineEntry {
  pub reference: HistoricalEventRef,
  pub from: ModelRef,
  pub to: ModelRef,
  pub kind: pi_rs_core::ModelFailureKind,
  pub gaps: Vec<CapabilityGap>,
  pub compacted: bool,
}

/// Counts of explicit reasoning provenance. No hidden reasoning is inferred.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceSummary {
  pub reasoning_chunks: u64,
  pub reasoning_bytes: u64,
  pub by_provenance: BTreeMap<ReasoningProvenance, ProvenanceCount>,
  pub native_present: bool,
  pub provider_summary_present: bool,
  pub declared_present: bool,
  pub reconstructed_present: bool,
  /// Always false: this crate never claims hidden chain-of-thought was recovered.
  pub hidden_reasoning_inferred: bool,
  pub redacted_events: u64,
  pub redactions: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProvenanceCount {
  pub chunks: u64,
  pub bytes: u64,
}

/// Export entry whose value omits raw payload references.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactedTraceExportEntry {
  pub reference: HistoricalEventRef,
  pub kind: EventKind,
  pub value: serde_json::Value,
}

/// Structural continuation comparison output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContinuationComparison {
  pub base: HistoricalEventRef,
  pub structurally_equal: bool,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub diverged_at: Option<usize>,
  pub left_len: usize,
  pub right_len: usize,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub left_at_divergence: Option<EventShape>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub right_at_divergence: Option<EventShape>,
  pub left_signature: Vec<EventShape>,
  pub right_signature: Vec<EventShape>,
}

/// Event shape used by structural comparison. Text content is deliberately excluded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventShape {
  pub kind: EventKind,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub model_epoch: Option<u32>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub model: Option<ModelRef>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub tool_name: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub tool_state: Option<ToolExecutionState>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub reasoning_provenance: Option<ReasoningProvenance>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub context_epoch: Option<u32>,
}

impl EventKind {
  pub fn of(event: &AgentEvent) -> Self {
    match event {
      AgentEvent::SessionStarted(_) => Self::SessionStarted,
      AgentEvent::UserMessage(_) => Self::UserMessage,
      AgentEvent::ModelRequestStarted(_) => Self::ModelRequestStarted,
      AgentEvent::ReasoningDelta(_) => Self::ReasoningDelta,
      AgentEvent::AssistantDelta(_) => Self::AssistantDelta,
      AgentEvent::ModelRequestCompleted(_) => Self::ModelRequestCompleted,
      AgentEvent::ModelRetry(_) => Self::ModelRetry,
      AgentEvent::ModelFailover(_) => Self::ModelFailover,
      AgentEvent::ModelEpochStarted(_) => Self::ModelEpochStarted,
      AgentEvent::ToolRequested(_) => Self::ToolRequested,
      AgentEvent::ToolStarted(_) => Self::ToolStarted,
      AgentEvent::ToolCompleted(_) => Self::ToolCompleted,
      AgentEvent::ToolFailed(_) => Self::ToolFailed,
      AgentEvent::ToolUnknown(_) => Self::ToolUnknown,
      AgentEvent::ExternalContextRetrieved(_) => Self::ExternalContextRetrieved,
      AgentEvent::ContextReduced(_) => Self::ContextReduced,
      AgentEvent::ContextCompactionStarted(_) => Self::ContextCompactionStarted,
      AgentEvent::ContextCompactionCompleted(_) => Self::ContextCompactionCompleted,
      AgentEvent::ContextSummary => Self::ContextSummary,
      AgentEvent::ContextCompactionEpoch(_) => Self::ContextCompactionEpoch,
      AgentEvent::CheckpointCreated(_) => Self::CheckpointCreated,
      AgentEvent::TurnCompleted(_) => Self::TurnCompleted,
      AgentEvent::Diagnostic(_) => Self::Diagnostic,
      AgentEvent::SessionEnded(_) => Self::SessionEnded,
    }
  }
}

fn ordered_entries(entries: &[TraceEntry]) -> Vec<&TraceEntry> {
  let mut indexed: Vec<(usize, &TraceEntry)> = entries.iter().enumerate().collect();
  indexed.sort_by_key(|(index, entry)| {
    (
      entry.envelope.meta.seq.unwrap_or(EventSeq(u64::MAX)),
      *index,
    )
  });
  indexed.into_iter().map(|(_, entry)| entry).collect()
}

fn apply_until<'a>(
  ordered: Vec<&'a TraceEntry>,
  until: &Option<HistoricalTarget>,
) -> Result<Vec<&'a TraceEntry>, ReplayError> {
  let Some(target) = until else {
    return Ok(ordered);
  };
  let mut bounded = Vec::new();
  for entry in ordered {
    let matches = target.matches(entry);
    bounded.push(entry);
    if matches {
      return Ok(bounded);
    }
  }
  Err(ReplayError::HistoricalTargetNotFound {
    target: target.clone(),
  })
}

fn is_zero(value: &u32) -> bool {
  *value == 0
}

fn is_tool_event(event: &AgentEvent) -> bool {
  matches!(
    event,
    AgentEvent::ToolRequested(_)
      | AgentEvent::ToolStarted(_)
      | AgentEvent::ToolCompleted(_)
      | AgentEvent::ToolFailed(_)
      | AgentEvent::ToolUnknown(_)
  )
}

fn is_reasoning_event(event: &AgentEvent) -> bool {
  matches!(
    event,
    AgentEvent::ReasoningDelta(_) | AgentEvent::ModelEpochStarted(_) | AgentEvent::ModelFailover(_)
  )
}

fn is_timing_event(event: &AgentEvent) -> bool {
  matches!(
    event,
    AgentEvent::ModelRequestStarted(_)
      | AgentEvent::ModelRequestCompleted(_)
      | AgentEvent::ModelRetry(_)
      | AgentEvent::ToolCompleted(_)
      | AgentEvent::ToolFailed(_)
      | AgentEvent::TurnCompleted(_)
      | AgentEvent::ContextCompactionStarted(_)
      | AgentEvent::ContextCompactionCompleted(_)
      | AgentEvent::CheckpointCreated(_)
      | AgentEvent::ModelFailover(_)
      | AgentEvent::ModelEpochStarted(_)
  )
}

fn timing_fact(entry: &TraceEntry) -> Option<TimingFact> {
  let duration_ms = match &entry.envelope.event {
    AgentEvent::ModelRequestCompleted(event) => Some(event.duration_ms),
    AgentEvent::ToolCompleted(event) => Some(event.duration_ms),
    AgentEvent::ToolFailed(event) => Some(event.duration_ms),
    AgentEvent::TurnCompleted(event) => Some(event.duration_ms),
    _ => None,
  };
  let retry_after_ms = match &entry.envelope.event {
    AgentEvent::ModelRetry(event) => event.retry_after_ms,
    _ => None,
  };
  if is_timing_event(&entry.envelope.event) || duration_ms.is_some() || retry_after_ms.is_some() {
    Some(TimingFact {
      timestamp_ms: entry.envelope.meta.timestamp_ms,
      duration_ms,
      retry_after_ms,
    })
  } else {
    None
  }
}

fn tool_replay_states_from_entries(entries: &[&TraceEntry]) -> Vec<ToolReplayState> {
  let mut states: BTreeMap<ToolCallId, ToolReplayState> = BTreeMap::new();
  for entry in entries {
    match &entry.envelope.event {
      AgentEvent::ToolRequested(event) => {
        states.insert(
          event.call_id.clone(),
          ToolReplayState {
            call_id: event.call_id.clone(),
            name: event.name.clone(),
            state: ToolExecutionState::Requested,
            read_only: event.read_only,
            mutating_unknown: false,
            reference: HistoricalEventRef::from_entry(entry),
            decision: if event.read_only {
              HistoricalToolDecision::SafeToReplayReadOnly
            } else {
              HistoricalToolDecision::RequestedOnly
            },
          },
        );
      }
      AgentEvent::ToolStarted(event) => update_tool_state(
        &mut states,
        entry,
        &event.call_id,
        &event.name,
        ToolExecutionState::Started,
        None,
      ),
      AgentEvent::ToolCompleted(event) => update_tool_state(
        &mut states,
        entry,
        &event.call_id,
        &event.name,
        event.state,
        None,
      ),
      AgentEvent::ToolFailed(event) => update_tool_state(
        &mut states,
        entry,
        &event.call_id,
        &event.name,
        ToolExecutionState::Failed,
        None,
      ),
      AgentEvent::ToolUnknown(event) => update_tool_state(
        &mut states,
        entry,
        &event.call_id,
        &event.name,
        ToolExecutionState::Unknown,
        Some(!event.mutating),
      ),
      _ => {}
    }
  }
  states.into_values().collect()
}

fn update_tool_state(
  states: &mut BTreeMap<ToolCallId, ToolReplayState>,
  entry: &TraceEntry,
  call_id: &ToolCallId,
  name: &str,
  state: ToolExecutionState,
  read_only_override: Option<bool>,
) {
  let existing_read_only = states
    .get(call_id)
    .map(|state| state.read_only)
    .unwrap_or(false);
  let read_only = read_only_override.unwrap_or(existing_read_only);
  let mutating_unknown = !read_only
    && matches!(
      state,
      ToolExecutionState::Started | ToolExecutionState::Unknown
    );
  let decision = match state {
    ToolExecutionState::Succeeded | ToolExecutionState::Failed => {
      HistoricalToolDecision::ReuseCommittedResult
    }
    ToolExecutionState::Requested => {
      if read_only {
        HistoricalToolDecision::SafeToReplayReadOnly
      } else {
        HistoricalToolDecision::RequestedOnly
      }
    }
    ToolExecutionState::Started | ToolExecutionState::Unknown => {
      if read_only {
        HistoricalToolDecision::SafeToReplayReadOnly
      } else {
        HistoricalToolDecision::ReconcileBeforeReplay
      }
    }
  };
  states.insert(
    call_id.clone(),
    ToolReplayState {
      call_id: call_id.clone(),
      name: name.to_string(),
      state,
      read_only,
      mutating_unknown,
      reference: HistoricalEventRef::from_entry(entry),
      decision,
    },
  );
}

fn model_epoch_timeline_from_entries(entries: &[&TraceEntry]) -> Vec<ModelEpochTimelineEntry> {
  entries
    .iter()
    .filter_map(|entry| match &entry.envelope.event {
      AgentEvent::ModelEpochStarted(event) => Some(ModelEpochTimelineEntry {
        reference: HistoricalEventRef::from_entry(entry),
        epoch: event.epoch,
        model: event.model.clone(),
        reason: event.reason.clone(),
        capabilities: event.capabilities.clone(),
      }),
      _ => None,
    })
    .collect()
}

fn compaction_timeline_from_entries(entries: &[&TraceEntry]) -> Vec<CompactionTimelineEntry> {
  entries
    .iter()
    .filter_map(|entry| match &entry.envelope.event {
      AgentEvent::ContextCompactionStarted(event) => Some(CompactionTimelineEntry::Started {
        reference: HistoricalEventRef::from_entry(entry),
        level: event.level,
        reason: event.reason.clone(),
      }),
      AgentEvent::ContextCompactionCompleted(event) => Some(CompactionTimelineEntry::Completed {
        reference: HistoricalEventRef::from_entry(entry),
        level: event.level,
        removed_messages: event.removed_messages,
        retained_messages: event.retained_messages,
        context_epoch: event.context_epoch,
      }),
      AgentEvent::ContextCompactionEpoch(event) => Some(CompactionTimelineEntry::Epoch {
        reference: HistoricalEventRef::from_entry(entry),
        context_epoch: event.context_epoch,
        replaces_from: event.replaces_from,
        replaces_through: event.replaces_through,
        summary: event.summary.clone(),
      }),
      AgentEvent::CheckpointCreated(event) => Some(CompactionTimelineEntry::Checkpoint {
        reference: HistoricalEventRef::from_entry(entry),
        checkpoint_id: event.checkpoint_id.clone(),
        capsule_version: event.capsule_version,
        summarized_events: event.summarized_events,
        path: event.path.clone(),
      }),
      _ => None,
    })
    .collect()
}

fn failover_timeline_from_entries(entries: &[&TraceEntry]) -> Vec<FailoverTimelineEntry> {
  entries
    .iter()
    .filter_map(|entry| match &entry.envelope.event {
      AgentEvent::ModelFailover(event) => Some(FailoverTimelineEntry {
        reference: HistoricalEventRef::from_entry(entry),
        from: event.from.clone(),
        to: event.to.clone(),
        kind: event.kind,
        gaps: event.gaps.clone(),
        compacted: event.compacted,
      }),
      _ => None,
    })
    .collect()
}

fn context_epoch_records(entries: &[TraceEntry]) -> Vec<ContextEpochRef> {
  ordered_entries(entries)
    .into_iter()
    .filter_map(|entry| match &entry.envelope.event {
      AgentEvent::ContextCompactionEpoch(epoch) => Some(ContextEpochRef {
        reference: HistoricalEventRef::from_entry(entry),
        context_epoch: epoch.context_epoch,
        replaces_from: epoch.replaces_from,
        replaces_through: epoch.replaces_through,
        summary: epoch.summary.clone(),
      }),
      _ => None,
    })
    .collect()
}

fn apply_context_epoch(working: &mut Vec<WorkingContextItem>, epoch: &ContextEpochRef) {
  let first = working.iter().position(|item| match item {
    WorkingContextItem::Message(message) => message
      .reference
      .seq
      .is_some_and(|seq| seq >= epoch.replaces_from && seq <= epoch.replaces_through),
    _ => false,
  });
  working.retain(|item| match item {
    WorkingContextItem::Message(message) => !message
      .reference
      .seq
      .is_some_and(|seq| seq >= epoch.replaces_from && seq <= epoch.replaces_through),
    _ => true,
  });
  if let Some(index) = first {
    // Runtime compaction persists the actual summary as a session message immediately before
    // the epoch marker. Preserve that exact model-visible message when it is available; the
    // opaque reference is only a fallback for trace-only reconstruction.
    let has_persisted_summary = working.iter().any(|item| match item {
      WorkingContextItem::Message(message) => message.reference.seq.is_some_and(|seq| {
        seq > epoch.replaces_through && seq < epoch.reference.seq.unwrap_or(EventSeq(u64::MAX))
      }),
      _ => false,
    });
    if !has_persisted_summary {
      working.insert(
        index,
        WorkingContextItem::CompactionSummary {
          context_epoch: epoch.context_epoch,
          replaces_from: epoch.replaces_from,
          replaces_through: epoch.replaces_through,
          summary: epoch.summary.clone(),
        },
      );
    }
  }
}

fn session_records_until(
  records: &[SessionRecord],
  target: Option<&HistoricalTarget>,
  bounded_trace: &[&TraceEntry],
) -> Vec<SessionRecord> {
  let Some(target) = target else {
    return records.to_vec();
  };
  let bounded_event_ids = bounded_trace
    .iter()
    .map(|entry| entry.envelope.meta.event_id.clone())
    .collect::<BTreeSet<_>>();
  records
    .iter()
    .filter(|record| match record {
      SessionRecord::Header(_) => true,
      SessionRecord::Message(message) => match target {
        HistoricalTarget::EventId(_) => bounded_event_ids.contains(&message.event_id),
        HistoricalTarget::Seq(limit) => {
          message.seq.is_some_and(|seq| seq <= *limit)
            || bounded_event_ids.contains(&message.event_id)
        }
      },
      SessionRecord::CheckpointBarrier(barrier) => bounded_trace.iter().any(|entry| {
        matches!(
          &entry.envelope.event,
          AgentEvent::CheckpointCreated(created) if created.checkpoint_id == barrier.checkpoint_id
        )
      }),
      SessionRecord::Epoch(epoch) => bounded_trace.iter().any(|entry| {
        matches!(
          &entry.envelope.event,
          AgentEvent::ModelEpochStarted(started) if started.epoch == epoch.epoch
        )
      }),
      SessionRecord::Compaction(compaction) => bounded_trace.iter().any(|entry| {
        matches!(
          &entry.envelope.event,
          AgentEvent::ContextCompactionCompleted(completed)
            if completed.context_epoch == compaction.context_epoch
        )
      }),
    })
    .cloned()
    .collect()
}

fn provenance_summary(
  entries: &[&TraceEntry],
  session_records: &[SessionRecord],
) -> ProvenanceSummary {
  let mut summary = ProvenanceSummary::default();
  for entry in entries {
    summary.redactions += u64::from(entry.redactions);
    if entry.redactions > 0 {
      summary.redacted_events += 1;
    }
    if let AgentEvent::ReasoningDelta(delta) = &entry.envelope.event {
      add_provenance(&mut summary, delta.provenance, delta.text.len() as u64);
    }
  }
  for record in session_records {
    if let SessionRecord::Message(message) = record {
      for chunk in message.message.reasoning() {
        add_provenance(&mut summary, chunk.provenance, chunk.text.len() as u64);
      }
    }
  }
  summary.hidden_reasoning_inferred = false;
  summary
}

fn add_provenance(summary: &mut ProvenanceSummary, provenance: ReasoningProvenance, bytes: u64) {
  let count = summary.by_provenance.entry(provenance).or_default();
  count.chunks += 1;
  count.bytes += bytes;
  summary.reasoning_chunks += 1;
  summary.reasoning_bytes += bytes;
  match provenance {
    ReasoningProvenance::Native => summary.native_present = true,
    ReasoningProvenance::ProviderSummary => summary.provider_summary_present = true,
    ReasoningProvenance::Declared => summary.declared_present = true,
    ReasoningProvenance::Reconstructed => summary.reconstructed_present = true,
  }
}

fn continuation_after(base: &HistoricalEventRef, entries: &[TraceEntry]) -> Vec<TraceEntry> {
  let ordered = ordered_entries(entries);
  if let Some(index) = ordered
    .iter()
    .position(|entry| entry.envelope.meta.event_id == base.event_id)
  {
    return ordered.into_iter().skip(index + 1).cloned().collect();
  }
  // A missing base id is intentionally not treated as evidence that records before `base.seq`
  // are historical. Branch traces may restart sequence numbers; dropping those records would
  // silently hide the beginning of a continuation. Callers comparing full histories must include
  // the base event in each input, while continuation-only inputs are accepted as-is.
  ordered.into_iter().cloned().collect()
}

fn structural_signature(entries: &[TraceEntry]) -> Vec<EventShape> {
  ordered_entries(entries).into_iter().map(shape).collect()
}

fn shape(entry: &TraceEntry) -> EventShape {
  let event = &entry.envelope.event;
  let (tool_name, tool_state) = match event {
    AgentEvent::ToolRequested(event) => (
      Some(event.name.clone()),
      Some(ToolExecutionState::Requested),
    ),
    AgentEvent::ToolStarted(event) => (Some(event.name.clone()), Some(ToolExecutionState::Started)),
    AgentEvent::ToolCompleted(event) => (Some(event.name.clone()), Some(event.state)),
    AgentEvent::ToolFailed(event) => (Some(event.name.clone()), Some(ToolExecutionState::Failed)),
    AgentEvent::ToolUnknown(event) => (Some(event.name.clone()), Some(ToolExecutionState::Unknown)),
    _ => (None, None),
  };
  let reasoning_provenance = match event {
    AgentEvent::ReasoningDelta(event) => Some(event.provenance),
    AgentEvent::ModelRequestCompleted(event) => event.reasoning_provenance,
    _ => None,
  };
  let context_epoch = match event {
    AgentEvent::ContextCompactionCompleted(event) => Some(event.context_epoch),
    AgentEvent::ContextCompactionEpoch(event) => Some(event.context_epoch),
    _ => None,
  };
  EventShape {
    kind: EventKind::of(event),
    model_epoch: entry.envelope.meta.model_epoch,
    model: entry.envelope.meta.model.clone(),
    tool_name,
    tool_state,
    reasoning_provenance,
    context_epoch,
  }
}

#[cfg(test)]
mod tests {
  use pi_rs_core::{
    AgentEvent, AssistantDelta, CapabilityGap, CheckpointCreated, ContextCompactionCompleted,
    ContextCompactionEpoch, ContextCompactionStarted, EventEnvelope, EventMeta,
    ExternalContextSource, ModelEpochStarted, ModelFailover, ModelRequestCompleted,
    ModelRequestStarted, ModelRetry, ReasoningDelta, SessionCompactionRecord, SessionHeader,
    SessionId, SessionStarted, ToolRequested, ToolStarted, ToolUnknown, TraceId, TurnId,
    UserMessage,
    context::{CAPSULE_SCHEMA_VERSION, CapsuleDecision},
    failure::ModelFailureKind,
    message::ContentBlock,
    session::SESSION_SCHEMA_VERSION,
  };

  use super::*;

  fn model() -> ModelRef {
    ModelRef::new("local", "qwen")
  }

  fn backup_model() -> ModelRef {
    ModelRef::new("backup", "gpt")
  }

  fn entry(seq: u64, event: AgentEvent) -> TraceEntry {
    let session_id = SessionId::from_string("session-1");
    let mut meta = EventMeta::new(session_id, TraceId::from_string("trace-1"));
    meta.seq = Some(EventSeq(seq));
    meta.timestamp_ms = 1_000 + seq;
    meta.model_epoch = Some(0);
    meta.model = Some(model());
    TraceEntry {
      envelope: EventEnvelope::new(meta, event),
      redactions: 0,
      raw_payload: false,
      raw_ref: None,
      externalized: Vec::new(),
    }
  }

  fn session_message(seq: u64, role: Role, message: Message) -> SessionRecord {
    SessionRecord::Message(SessionMessage {
      turn_id: TurnId::from_string(format!("turn-{seq}")),
      role,
      message,
      epoch: 0,
      model: model(),
      event_id: EventId::from_string(format!("event-{seq}")),
      seq: Some(EventSeq(seq)),
      external_context: None,
    })
  }

  #[test]
  fn replay_orders_by_sequence_and_stops_inclusively() {
    let entries = vec![
      entry(
        3,
        AgentEvent::AssistantDelta(AssistantDelta {
          text: "hello".into(),
          chunk_index: 0,
        }),
      ),
      entry(
        1,
        AgentEvent::UserMessage(UserMessage {
          text: "start".into(),
          attachments: 0,
        }),
      ),
      entry(
        2,
        AgentEvent::ModelRequestStarted(ModelRequestStarted {
          epoch: 0,
          model: model(),
          message_count: 1,
          context_tokens_est: 10,
          tools_exposed: 0,
        }),
      ),
    ];
    let report = replay_trace(
      &entries,
      &ReplayOptions {
        filters: ReplayFilters::all(),
        until: Some(HistoricalTarget::Seq(EventSeq(2))),
      },
    )
    .unwrap();
    assert_eq!(report.events.len(), 2);
    assert_eq!(report.events[0].reference.seq, Some(EventSeq(1)));
    assert_eq!(report.events[1].reference.seq, Some(EventSeq(2)));
  }

  #[test]
  fn event_id_cutoff_excludes_seq_less_session_messages_after_the_target() {
    let first = entry(
      1,
      AgentEvent::UserMessage(UserMessage {
        text: "first".into(),
        attachments: 0,
      }),
    );
    let mut target = entry(
      2,
      AgentEvent::UserMessage(UserMessage {
        text: "target".into(),
        attachments: 0,
      }),
    );
    let mut future = entry(
      3,
      AgentEvent::ReasoningDelta(ReasoningDelta {
        text: "future".into(),
        provenance: ReasoningProvenance::Declared,
        chunk_index: 0,
      }),
    );
    target.envelope.meta.seq = None;
    future.envelope.meta.seq = None;
    let session = vec![
      session_message(1, Role::User, Message::user("first")),
      session_message(2, Role::User, Message::user("target")),
      session_message(3, Role::Assistant, Message::assistant("future")),
    ];
    let mut session = session;
    for (record, trace) in session.iter_mut().zip([&first, &target, &future]) {
      let SessionRecord::Message(message) = record else {
        unreachable!()
      };
      message.event_id = trace.envelope.meta.event_id.clone();
      if trace.envelope.meta.seq.is_none() {
        message.seq = None;
      }
    }
    let report = replay_trace_with_session(
      &[first.clone(), target.clone(), future],
      &session,
      &ReplayOptions {
        filters: ReplayFilters::all(),
        until: Some(HistoricalTarget::EventId(
          target.envelope.meta.event_id.clone(),
        )),
      },
    )
    .unwrap();
    assert_eq!(report.provenance.reasoning_chunks, 0);
    let plan = plan_historical_branch(
      &[first.clone(), target.clone()],
      &session,
      HistoricalTarget::EventId(target.envelope.meta.event_id.clone()),
    )
    .unwrap();
    assert_eq!(plan.context.canonical.messages.len(), 2);
    assert!(
      plan
        .context
        .canonical
        .messages
        .iter()
        .all(|message| message.message.text() != "future")
    );
  }

  #[test]
  fn replay_filters_tools_reasoning_and_timing() {
    let call_id = ToolCallId::from_string("tool-1");
    let entries = vec![
      entry(
        1,
        AgentEvent::ReasoningDelta(ReasoningDelta {
          text: "visible reasoning".into(),
          provenance: ReasoningProvenance::Native,
          chunk_index: 0,
        }),
      ),
      entry(
        2,
        AgentEvent::ToolRequested(ToolRequested {
          call_id: call_id.clone(),
          name: "read".into(),
          arguments: serde_json::json!({"path":"src/lib.rs"}),
          read_only: true,
        }),
      ),
      entry(
        3,
        AgentEvent::ModelRequestCompleted(ModelRequestCompleted {
          epoch: 0,
          model: model(),
          finish_reason: Some("stop".into()),
          input_tokens: None,
          output_tokens: None,
          duration_ms: 42,
          tool_calls: 0,
          reasoning_provenance: None,
          first_delta_ms: None,
        }),
      ),
    ];
    let tools = replay_trace(
      &entries,
      &ReplayOptions {
        filters: ReplayFilters::tools(),
        until: None,
      },
    )
    .unwrap();
    assert_eq!(tools.events.len(), 1);
    assert_eq!(tools.events[0].kind, EventKind::ToolRequested);

    let reasoning = replay_trace(
      &entries,
      &ReplayOptions {
        filters: ReplayFilters::reasoning(),
        until: None,
      },
    )
    .unwrap();
    assert_eq!(reasoning.events[0].kind, EventKind::ReasoningDelta);

    let timing = replay_trace(
      &entries,
      &ReplayOptions {
        filters: ReplayFilters::timing(),
        until: None,
      },
    )
    .unwrap();
    assert_eq!(timing.events.len(), 1);
    assert_eq!(
      timing.events[0].timing.as_ref().unwrap().duration_ms,
      Some(42)
    );
  }

  #[test]
  fn context_reconstruction_keeps_canonical_history_separate_from_working_context() {
    let summary = BlobRef::for_bytes(b"summary", Some("text/plain"));
    let trace = vec![entry(
      6,
      AgentEvent::ContextCompactionEpoch(ContextCompactionEpoch {
        context_epoch: 1,
        replaces_from: EventSeq(1),
        replaces_through: EventSeq(2),
        summary: Some(summary.clone()),
      }),
    )];
    let session = vec![
      session_message(1, Role::User, Message::user("old user")),
      session_message(2, Role::Assistant, Message::assistant("old answer")),
      session_message(7, Role::User, Message::user("new user")),
    ];
    let context = reconstruct_context(&session, &trace);
    assert_eq!(
      context.canonical.messages.len(),
      3,
      "canonical messages stay intact"
    );
    assert_eq!(context.working.context_epoch, 1);
    assert!(matches!(
      context.working.items[0],
      WorkingContextItem::CompactionSummary { .. }
    ));
    assert!(matches!(
      context.working.items[1],
      WorkingContextItem::Message(_)
    ));
  }

  #[test]
  fn persisted_compaction_summary_is_not_duplicated_by_an_opaque_reference() {
    let summary = BlobRef::for_bytes(b"summary", Some("text/plain"));
    let trace = vec![entry(
      4,
      AgentEvent::ContextCompactionEpoch(ContextCompactionEpoch {
        context_epoch: 1,
        replaces_from: EventSeq(1),
        replaces_through: EventSeq(2),
        summary: Some(summary),
      }),
    )];
    let session = vec![
      session_message(1, Role::User, Message::user("old")),
      session_message(2, Role::Assistant, Message::assistant("answer")),
      session_message(3, Role::User, Message::user("persisted summary")),
    ];
    let context = reconstruct_context(&session, &trace);
    assert_eq!(context.working.items.len(), 1);
    let WorkingContextItem::Message(message) = &context.working.items[0] else {
      panic!("expected the persisted summary message")
    };
    assert_eq!(message.message.text(), "persisted summary");
  }

  #[test]
  fn checkpoint_barrier_resets_working_context_to_capsule_plus_tail() {
    let mut capsule = ContextCapsule::new("ship replay");
    capsule.current_state = "tests passing".into();
    capsule.decisions.push(CapsuleDecision {
      decision: "branching is dry".into(),
      rationale: "replay must not execute tools".into(),
    });
    let session = vec![
      SessionRecord::Header(SessionHeader {
        session_id: SessionId::from_string("session-1"),
        version: SESSION_SCHEMA_VERSION,
        started_at_ms: 1,
        working_dir: "/repo".into(),
        model: model(),
        parent_session: None,
        branched_from_event: None,
        imported_from: None,
      }),
      session_message(1, Role::User, Message::user("old")),
      SessionRecord::CheckpointBarrier(pi_rs_core::SessionCheckpointRecord {
        checkpoint_id: CheckpointId::from_string("checkpoint-1"),
        capsule_version: CAPSULE_SCHEMA_VERSION,
        capsule_path: "checkpoints/1.json".into(),
        capsule: capsule.clone(),
      }),
      session_message(5, Role::User, Message::user("tail")),
    ];
    let context = reconstruct_context(&session, &[]);
    assert_eq!(context.canonical.messages.len(), 2);
    assert_eq!(context.working.items.len(), 2);
    let WorkingContextItem::CheckpointCapsule { capsule_text, .. } = &context.working.items[0]
    else {
      panic!("expected capsule first")
    };
    assert!(capsule_text.contains("objective: ship replay"));
  }

  #[test]
  fn session_compaction_record_retains_tail_when_no_trace_epoch_is_available() {
    let session = vec![
      session_message(1, Role::User, Message::user("old")),
      session_message(2, Role::Assistant, Message::assistant("old answer")),
      SessionRecord::Compaction(SessionCompactionRecord {
        context_epoch: 1,
        level: ContextLevel::L1Ordinary,
        removed_messages: 2,
        retained_from: 2,
        retained_messages: 0,
        summary_present: false,
        replaces_from: None,
        replaces_through: None,
      }),
      session_message(5, Role::User, Message::user("tail")),
    ];
    let context = reconstruct_context(&session, &[]);
    assert_eq!(context.canonical.messages.len(), 3);
    assert_eq!(context.working.items.len(), 1);
    assert_eq!(context.working.context_epoch, 1);
  }

  #[test]
  fn branch_plan_is_dry_and_blocks_mutating_unknown_tools() {
    let call_id = ToolCallId::from_string("tool-1");
    let trace = vec![
      entry(
        1,
        AgentEvent::ToolRequested(ToolRequested {
          call_id: call_id.clone(),
          name: "write".into(),
          arguments: serde_json::json!({"path":"x","contents":"y"}),
          read_only: false,
        }),
      ),
      entry(
        2,
        AgentEvent::ToolStarted(ToolStarted {
          call_id: call_id.clone(),
          name: "write".into(),
        }),
      ),
      entry(
        3,
        AgentEvent::ToolUnknown(ToolUnknown {
          call_id: call_id.clone(),
          name: "write".into(),
          why: "completion not observed".into(),
          mutating: true,
        }),
      ),
    ];
    let plan = plan_historical_branch(&trace, &[], HistoricalTarget::Seq(EventSeq(3))).unwrap();
    assert!(plan.new_execution.planned_only);
    assert!(plan.new_execution.history_separated);
    assert_eq!(plan.blocked_tools.len(), 1);
    assert_eq!(
      plan.blocked_tools[0].decision,
      HistoricalToolDecision::ReconcileBeforeReplay
    );
  }

  #[test]
  fn continuation_comparison_reports_first_structural_divergence_without_text_judgment() {
    let base = HistoricalEventRef::from_entry(&entry(
      1,
      AgentEvent::UserMessage(UserMessage {
        text: "base".into(),
        attachments: 0,
      }),
    ));
    let left = vec![entry(
      2,
      AgentEvent::AssistantDelta(AssistantDelta {
        text: "hello".into(),
        chunk_index: 0,
      }),
    )];
    let right = vec![entry(
      2,
      AgentEvent::ReasoningDelta(ReasoningDelta {
        text: "thinking".into(),
        provenance: ReasoningProvenance::ProviderSummary,
        chunk_index: 0,
      }),
    )];
    let comparison = compare_continuations(base, &left, &right);
    assert!(!comparison.structurally_equal);
    assert_eq!(comparison.diverged_at, Some(0));
    assert_eq!(
      comparison.left_at_divergence.unwrap().kind,
      EventKind::AssistantDelta
    );
    assert_eq!(
      comparison.right_at_divergence.unwrap().reasoning_provenance,
      Some(ReasoningProvenance::ProviderSummary)
    );
  }

  #[test]
  fn continuation_comparison_slices_full_histories_after_the_base() {
    let base_entry = entry(
      2,
      AgentEvent::UserMessage(UserMessage {
        text: "base".into(),
        attachments: 0,
      }),
    );
    let base = HistoricalEventRef::from_entry(&base_entry);
    let left = vec![
      entry(
        1,
        AgentEvent::UserMessage(UserMessage {
          text: "before".into(),
          attachments: 0,
        }),
      ),
      base_entry.clone(),
      entry(
        3,
        AgentEvent::AssistantDelta(AssistantDelta {
          text: "left".into(),
          chunk_index: 0,
        }),
      ),
    ];
    let right = vec![
      entry(
        1,
        AgentEvent::UserMessage(UserMessage {
          text: "different before".into(),
          attachments: 0,
        }),
      ),
      base_entry,
      entry(
        3,
        AgentEvent::AssistantDelta(AssistantDelta {
          text: "right".into(),
          chunk_index: 0,
        }),
      ),
    ];
    let comparison = compare_continuations(base, &left, &right);
    assert!(comparison.structurally_equal);
    assert_eq!(comparison.left_len, 1);
    assert_eq!(comparison.right_len, 1);
  }

  #[test]
  fn continuation_only_traces_keep_restarted_sequence_numbers() {
    let base = HistoricalEventRef {
      session_id: SessionId::from_string("original"),
      event_id: EventId::from_string("base-event"),
      seq: Some(EventSeq(10)),
    };
    let left = vec![entry(
      1,
      AgentEvent::AssistantDelta(AssistantDelta {
        text: "left".into(),
        chunk_index: 0,
      }),
    )];
    let right = vec![entry(
      1,
      AgentEvent::AssistantDelta(AssistantDelta {
        text: "right".into(),
        chunk_index: 0,
      }),
    )];
    let comparison = compare_continuations(base, &left, &right);
    assert!(comparison.structurally_equal);
    assert_eq!(comparison.left_len, 1);
    assert_eq!(comparison.right_len, 1);
  }

  #[test]
  fn timelines_collect_epochs_compactions_and_failovers() {
    let mut epoch_entry = entry(
      1,
      AgentEvent::ModelEpochStarted(ModelEpochStarted {
        epoch: 0,
        model: model(),
        reason: EpochReason::Initial,
        capabilities: ModelCapabilities::text_only(32_000),
      }),
    );
    epoch_entry.envelope.meta.model_epoch = Some(0);
    let entries = vec![
      epoch_entry,
      entry(
        2,
        AgentEvent::ModelRetry(ModelRetry {
          attempt: 1,
          max_attempts: 2,
          kind: ModelFailureKind::ProviderUnavailable,
          retry_after_ms: Some(100),
          will_failover: true,
        }),
      ),
      entry(
        3,
        AgentEvent::ModelFailover(ModelFailover {
          from: model(),
          to: backup_model(),
          kind: ModelFailureKind::ProviderUnavailable,
          gaps: vec![CapabilityGap::ContextWindow {
            required: 32_000,
            available: 16_000,
          }],
          compacted: true,
        }),
      ),
      entry(
        4,
        AgentEvent::ContextCompactionStarted(ContextCompactionStarted {
          level: ContextLevel::L1Ordinary,
          reason: "fit backup".into(),
        }),
      ),
      entry(
        5,
        AgentEvent::ContextCompactionCompleted(ContextCompactionCompleted {
          level: ContextLevel::L1Ordinary,
          removed_messages: 4,
          retained_messages: 2,
          context_epoch: 1,
        }),
      ),
      entry(
        6,
        AgentEvent::CheckpointCreated(CheckpointCreated {
          checkpoint_id: CheckpointId::from_string("checkpoint-1"),
          capsule_version: CAPSULE_SCHEMA_VERSION,
          summarized_events: 6,
          path: "checkpoints/1.json".into(),
        }),
      ),
    ];
    assert_eq!(model_epoch_timeline(&entries).len(), 1);
    assert_eq!(failover_timeline(&entries).len(), 1);
    assert_eq!(compaction_timeline(&entries).len(), 3);
    let report = replay_trace(
      &entries,
      &ReplayOptions {
        filters: ReplayFilters::timing(),
        until: None,
      },
    )
    .unwrap();
    assert_eq!(report.failovers[0].to, backup_model());
    assert_eq!(
      report.events.iter().find_map(|event| event
        .timing
        .as_ref()
        .and_then(|timing| timing.retry_after_ms)),
      Some(100)
    );
  }

  #[test]
  fn provenance_summary_counts_only_explicit_claims_and_never_infers_hidden_reasoning() {
    let mut redacted = entry(
      1,
      AgentEvent::ReasoningDelta(ReasoningDelta {
        text: "summary".into(),
        provenance: ReasoningProvenance::ProviderSummary,
        chunk_index: 0,
      }),
    );
    redacted.redactions = 2;
    let session = vec![SessionRecord::Message(SessionMessage {
      turn_id: TurnId::from_string("turn-1"),
      role: Role::Assistant,
      message: Message::new(
        Role::Assistant,
        vec![ContentBlock::Reasoning(pi_rs_core::ReasoningChunk::new(
          "declared rationale",
          ReasoningProvenance::Declared,
        ))],
      ),
      epoch: 0,
      model: model(),
      event_id: EventId::from_string("event-session"),
      seq: Some(EventSeq(2)),
      external_context: None,
    })];
    let summary = summarize_provenance(&[redacted], &session);
    assert_eq!(summary.reasoning_chunks, 2);
    assert!(summary.provider_summary_present);
    assert!(summary.declared_present);
    assert!(!summary.native_present);
    assert!(!summary.hidden_reasoning_inferred);
    assert_eq!(summary.redacted_events, 1);
    assert_eq!(summary.redactions, 2);
  }

  #[test]
  fn redacted_export_omits_raw_payload_reference_but_keeps_redaction_facts() {
    let mut trace = entry(
      1,
      AgentEvent::ExternalContextRetrieved(pi_rs_core::ExternalContextRetrieved {
        source: ExternalContextSource {
          provider: "rkb-rs".into(),
          resource_id: "chunk-1".into(),
          provenance: "rkb-rs/agent-context".into(),
        },
        citation: Some("[1]".into()),
        bytes: 12,
        inline: true,
        metadata: BTreeMap::new(),
      }),
    );
    trace.redactions = 1;
    trace.raw_payload = true;
    trace.raw_ref = Some("blobs/aa/raw-secret".into());
    let exported = export_redacted_trace(&[trace], &ReplayFilters::all());
    let value = &exported[0].value;
    assert_eq!(value["redactions"], serde_json::json!(1));
    assert_eq!(value["raw_payload_attached"], serde_json::json!(true));
    assert!(
      value.get("raw_ref").is_none(),
      "raw payload pointers are not exported: {value}"
    );
    assert_eq!(exported[0].kind, EventKind::ExternalContextRetrieved);
  }

  #[test]
  fn missing_until_target_is_an_error_not_a_silent_full_replay() {
    let err = replay_trace(
      &[entry(
        1,
        AgentEvent::SessionStarted(SessionStarted {
          working_dir: "/repo".into(),
          model: model(),
          capabilities: ModelCapabilities::text_only(8_000),
          resumed: false,
        }),
      )],
      &ReplayOptions {
        filters: ReplayFilters::all(),
        until: Some(HistoricalTarget::Seq(EventSeq(99))),
      },
    )
    .unwrap_err();
    assert!(matches!(err, ReplayError::HistoricalTargetNotFound { .. }));
  }
}
