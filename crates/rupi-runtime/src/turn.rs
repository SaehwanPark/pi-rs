//! The turn loop: one user input, driven to a terminal state.
//!
//! A turn is the unit the user experiences and the unit the trace must
//! reconstruct. This module owns the sequence and nothing else — no wire format,
//! no side effects, no rendering. Those live behind the traits below, which is
//! what lets the same loop run against a real provider, a scripted one in a
//! test, or no provider at all.
//!
//! The invariants it defends:
//!
//! - **One model at a time.** Exactly one epoch is active; a change of model is an
//!   epoch transition with a recorded reason, never an implicit swap.
//! - **A request is re-issued only while nothing has been streamed.** Once output
//!   reaches the user it is committed content; restarting would duplicate a
//!   half-answer, which is worse than a visible failure.
//! - **Every tool call reaches a terminal lifecycle state**, including calls that
//!   were interrupted or refused. A tool call with no terminal event is a bug here.
//! - **Context is never silently truncated.** An oversized request is refused, and
//!   the refusal is recorded.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use rupi_core::{
  AgentEvent, AssistantDelta, AttributedMessage, BlobRef, CancelToken, CapabilityGap,
  CapsuleArtifact, CapsuleDecision, CheckpointCreated, CheckpointId, ContentBlock, ContextAction,
  ContextCapsule, ContextCompactionCompleted, ContextCompactionEpoch, ContextCompactionStarted,
  ContextLevel, ContextPolicy, ContextReduced, ContextState, DEFAULT_MAX_MODEL_REQUESTS_PER_TURN,
  Diagnostic, DiagnosticLevel, EpochReason, EventEnvelope, EventMeta, EventSeq, EventSink,
  ExternalContextItem, ExternalContextRetrieved, FailurePhase,
  MAX_CONFIGURED_MODEL_REQUESTS_PER_TURN, Message, ModelCapabilities, ModelEpoch,
  ModelEpochStarted, ModelFailover, ModelFailure, ModelFailureKind, ModelProvider, ModelRef,
  ModelRequest, ModelRequestCompleted, ModelRequestStarted, ModelRetry, ReasoningDelta,
  ReasoningProvenance, ReductionReason, Role, SessionEndReason, SessionEnded, SessionId,
  SessionStarted, SinkError, ThinkingLevel, ToolCallBlock, ToolCompleted, ToolExecutionState,
  ToolFailed, ToolOutcome, ToolProgress, ToolRequested, ToolResultBlock, ToolStarted, ToolUnknown,
  TraceId, TurnCompleted, TurnId, TurnStatus, UserMessage,
};
use rupi_tools::{Approval, ApprovalGate, Executed, ToolRegistry};

use crate::failover::{FailoverPolicy, Recovery};

/// How to handle context pressure when policy recommends compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompactionStrategy {
  /// Drop oldest turns without summarizing (pre-emptive reduction).
  #[default]
  Evict,
  /// Summarize oldest turns into a canonical summary and open a compaction epoch.
  Summarize,
}

/// Custom summarizer function alias.
pub type Summarizer = Arc<dyn Fn(&[Message]) -> String + Send + Sync>;

/// How to handle context pressure when policy suggests a checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CheckpointStrategy {
  /// Automatically synthesize a context capsule and create a durable checkpoint.
  #[default]
  Auto,
  /// Do not automatically create checkpoints (diagnostics only).
  Disabled,
}

/// Custom checkpointer function alias.
pub type Checkpointer = Arc<dyn Fn(&[Message], &ContextState) -> ContextCapsule + Send + Sync>;

/// Durable state required to reopen a loop without resetting its model or context
/// timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeState {
  /// The model-visible window, already reduced past the latest checkpoint and
  /// compaction boundaries.
  pub messages: Vec<Message>,
  /// Canonical sequence for each visible message when it came from durable
  /// history. `None` denotes an in-memory/system capsule message.
  pub message_seqs: Vec<Option<EventSeq>>,
  /// All model epochs in durable order, including the active epoch.
  pub epochs: Vec<ModelEpoch>,
  /// Next context-compaction epoch to continue from.
  pub context_epoch: u32,
  /// Leading messages that belong to the latest checkpoint capsule and may not
  /// be crossed by ordinary compaction.
  pub checkpoint_floor: usize,
  /// Canonical event range inherited from the prior process, used by the next
  /// compaction to cite the history it replaces.
  pub cited_history: Option<(EventSeq, EventSeq)>,
  /// Tool calls whose terminal event was absent when the prior process stopped.
  /// These must be reconciled before a new provider request.
  pub interrupted_tools: Vec<rupi_core::InterruptedToolCall>,
}

/// How many model round-trips one user input may take.
///
/// A model that keeps asking for tools is looping; the limit exists so a loop
/// costs one visible failure instead of an unbounded bill.
pub const MAX_MODEL_REQUESTS_PER_TURN: usize = DEFAULT_MAX_MODEL_REQUESTS_PER_TURN as usize;

/// Live feedback for the surface. Every method is optional by design: the loop
/// must be runnable with nobody watching.
pub trait TurnProgress: Send {
  fn on_user_message(&mut self, _text: &str) {}
  fn on_request_started(&mut self, _model: &ModelRef) {}
  /// Report a request with its position in the current turn's budget.
  ///
  /// The compatibility default keeps existing integrations that only implement
  /// `on_request_started` source-compatible while allowing user-facing surfaces
  /// to show sparse near-limit progress.
  fn on_request_started_with_budget(&mut self, model: &ModelRef, _request: usize, _max: usize) {
    self.on_request_started(model);
  }
  fn on_reasoning(&mut self, _text: &str, _provenance: ReasoningProvenance) {}
  fn on_text_delta(&mut self, _text: &str) {}
  fn on_tool_requested(&mut self, _call: &ToolCallBlock) {}
  /// Answer a mutating-tool approval request on a surface that can ask a person.
  ///
  /// The safe default refuses. Interactive approvals must be explicitly enabled
  /// on the turn loop and implemented by the attached surface.
  fn approve_mutating_tool(
    &mut self,
    _metadata: &rupi_core::ToolMetadata,
    _arguments: &serde_json::Value,
  ) -> Approval {
    Approval::Deny("this surface cannot approve mutating tools; nothing was changed".into())
  }
  fn on_tool_progress(&mut self, _call: &ToolCallBlock, _text: &str) {}
  fn on_tool_finished(&mut self, _call: &ToolCallBlock, _executed: &Executed) {}
}

/// A sink that records nothing, for headless runs and tests.
pub struct SilentProgress;
impl TurnProgress for SilentProgress {}

/// Durable sink for the canonical event stream.
///
/// The loop emits *semantic* events and does not know whether they reach a file, a
/// test, or both. Sequence numbers are assigned by the store, so the loop leaves
/// `meta.seq` unset.
pub trait Trace: Send {
  /// Deliver one event. A durable implementation stamps its assigned sequence
  /// back into the envelope.
  fn emit(&mut self, envelope: &mut EventEnvelope) -> Result<(), SinkError>;

  /// Deliver a terminal event that intentionally has no model-visible message.
  /// Durable sinks can commit its transaction immediately instead of leaving a
  /// held message intent that has no caller to complete.
  fn emit_without_message(&mut self, envelope: &mut EventEnvelope) -> Result<(), SinkError> {
    self.emit(envelope)
  }

  /// Close an emitted terminal event whose provider response produced no
  /// model-visible message. Durable sinks use this to commit a held WAL intent;
  /// trace-only sinks have nothing to do.
  fn complete_without_message(&mut self, _envelope: &EventEnvelope) -> Result<(), SinkError> {
    Ok(())
  }

  /// Emit one canonical event and its exact model-visible message as one logical
  /// durable operation. The store-backed implementation prepares the recovery
  /// payload before appending the event; non-durable sinks use the compatibility
  /// sequence below.
  fn emit_message(
    &mut self,
    envelope: &mut EventEnvelope,
    message: &Message,
  ) -> Result<(), SinkError> {
    if envelope.meta.seq.is_none() {
      self.emit(envelope)?;
    }
    self.record_message(&AttributedMessage {
      envelope: envelope.clone(),
      message: message.clone(),
    })
  }

  /// Compatibility escape hatch for callers that already emitted an event.
  /// Runtime message-bearing paths use [`Trace::emit_message`] so a durable
  /// implementation can keep the event and projection in one transaction.
  fn record_message(&mut self, attributed: &AttributedMessage) -> Result<(), SinkError> {
    let _ = attributed;
    Ok(())
  }

  /// Persist model-visible bytes so a reduction can be recovered.
  ///
  /// Default: no-op. The store-backed implementation records the full payload and
  /// returns the reference the `context_reduced` event carries, which is what
  /// keeps "the model saw a summary" reversible.
  fn put_payload(&mut self, bytes: &[u8]) -> Result<Option<BlobRef>, SinkError> {
    let _ = bytes;
    Ok(None)
  }

  /// Persist a checkpoint capsule and barrier, returning the checkpoint ID and relative path if supported.
  fn create_checkpoint(
    &mut self,
    capsule: &ContextCapsule,
  ) -> Result<Option<(CheckpointId, String)>, SinkError> {
    let _ = capsule;
    Ok(None)
  }

  /// Attach the context epoch that will become active at the checkpoint
  /// boundary. Durable sinks use this before publishing `CheckpointCreated`.
  fn set_checkpoint_context_epoch(&mut self, _context_epoch: u32) -> Result<(), SinkError> {
    Ok(())
  }

  /// List checkpoint capsules recorded for this session if supported.
  fn list_checkpoints(&self) -> Result<Vec<(CheckpointId, ContextCapsule)>, SinkError> {
    Ok(Vec::new())
  }

  /// Push buffered events to their final destination.
  fn flush(&mut self) -> Result<(), SinkError> {
    Ok(())
  }
}

impl<T: Trace + ?Sized> Trace for &mut T {
  fn emit(&mut self, envelope: &mut EventEnvelope) -> Result<(), SinkError> {
    <T as Trace>::emit(self, envelope)
  }

  fn emit_without_message(&mut self, envelope: &mut EventEnvelope) -> Result<(), SinkError> {
    <T as Trace>::emit_without_message(self, envelope)
  }

  fn complete_without_message(&mut self, envelope: &EventEnvelope) -> Result<(), SinkError> {
    <T as Trace>::complete_without_message(self, envelope)
  }

  fn emit_message(
    &mut self,
    envelope: &mut EventEnvelope,
    message: &Message,
  ) -> Result<(), SinkError> {
    <T as Trace>::emit_message(self, envelope, message)
  }

  fn record_message(&mut self, attributed: &AttributedMessage) -> Result<(), SinkError> {
    <T as Trace>::record_message(self, attributed)
  }

  fn put_payload(&mut self, bytes: &[u8]) -> Result<Option<BlobRef>, SinkError> {
    <T as Trace>::put_payload(self, bytes)
  }

  fn create_checkpoint(
    &mut self,
    capsule: &ContextCapsule,
  ) -> Result<Option<(CheckpointId, String)>, SinkError> {
    <T as Trace>::create_checkpoint(self, capsule)
  }

  fn set_checkpoint_context_epoch(&mut self, context_epoch: u32) -> Result<(), SinkError> {
    <T as Trace>::set_checkpoint_context_epoch(self, context_epoch)
  }

  fn list_checkpoints(&self) -> Result<Vec<(CheckpointId, ContextCapsule)>, SinkError> {
    (**self).list_checkpoints()
  }

  fn flush(&mut self) -> Result<(), SinkError> {
    <T as Trace>::flush(self)
  }
}

/// Adapts any [`EventSink`] into a [`Trace`].
///
/// The store's session and the runtime's sink are the same thing viewed from two
/// sides; this keeps that mapping in one place instead of at every call site.
pub struct TraceSink<S: EventSink>(pub S);

impl<S: EventSink> Trace for TraceSink<S> {
  fn emit(&mut self, envelope: &mut EventEnvelope) -> Result<(), SinkError> {
    self.0.emit(envelope)
  }

  fn flush(&mut self) -> Result<(), SinkError> {
    EventSink::flush(&mut self.0)
  }
}

/// A turn-level failure, already recorded as events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnError {
  /// No model could produce a usable response. Carries the last failure, which is
  /// the one worth showing.
  Unavailable(ModelFailure),
  /// The turn stopped for a reason the user should see.
  Aborted(TurnStatus),
  /// Durable recording failed. A harness may not continue blind.
  Sink(String),
  /// A management request was rejected before changing durable or live state.
  Refused(String),
}

impl From<SinkError> for TurnError {
  fn from(error: SinkError) -> Self {
    Self::Sink(error.to_string())
  }
}

impl TurnError {
  /// The failure kind, when the turn ended because no model could serve it.
  ///
  /// Callers branch on this: an availability failure is worth a retry prompt, a
  /// semantic one is not.
  pub fn kind(&self) -> Option<ModelFailureKind> {
    match self {
      Self::Unavailable(failure) => Some(failure.kind),
      Self::Aborted(status) => match status {
        TurnStatus::Failed { kind } => Some(*kind),
        TurnStatus::Completed | TurnStatus::Cancelled | TurnStatus::BudgetExhausted => None,
      },
      Self::Sink(_) | Self::Refused(_) => None,
    }
  }

  /// `true` when the session may be continued after this failure.
  ///
  /// A sink failure says no: the runtime could not record what it was doing, so
  /// continuing would build on history it cannot trust.
  pub fn session_recoverable(&self) -> bool {
    !matches!(self, Self::Sink(_))
  }
}

/// Where a turn ended up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnReport {
  pub turn_id: TurnId,
  pub status: TurnStatus,
  /// Assistant text committed to the session.
  pub text: String,
  /// Tool calls that reached a terminal state.
  pub tool_calls: u32,
  /// Model round-trips used.
  pub requests: usize,
  /// Epoch active when the turn ended, so a caller can report *which* model
  /// answered.
  pub epoch: u32,
  pub duration_ms: u64,
  /// `true` when the loop stopped because the request budget ran out rather than
  /// because the model produced an answer.
  pub budget_exhausted: bool,
}

impl TurnReport {
  fn new(turn_id: TurnId, epoch: u32) -> Self {
    Self {
      turn_id,
      status: TurnStatus::Completed,
      text: String::new(),
      tool_calls: 0,
      requests: 0,
      epoch,
      duration_ms: 0,
      budget_exhausted: false,
    }
  }
}

/// One active model, with the capability snapshot validated when it became active.
#[derive(Debug, Clone)]
struct Epoch {
  index: u32,
  model: ModelRef,
  capabilities: ModelCapabilities,
  reason: EpochReason,
}

/// What the runtime does after consulting the failover policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
  /// Re-issue against the same model.
  Retry,
  /// Re-issue against the backup, after the epoch transition was recorded.
  Takeover,
  /// Stop the turn.
  Stop,
}

/// What one model request produced.
struct Response {
  epoch: u32,
  text: Option<String>,
  calls: Vec<ToolCallBlock>,
  rejected_calls: BTreeMap<String, String>,
  /// The completion is held until the exact assistant message is assembled, so
  /// the canonical completion and its projection share one transaction.
  completion: EventEnvelope,
}

struct RecordedResponse {
  assistant_event_id: rupi_core::EventId,
  calls: Vec<ToolCallBlock>,
  rejected_calls: BTreeMap<String, String>,
}

/// Drives turns against one primary model, with an optional backup.
///
/// Long-lived on purpose: it owns the epoch list, so a failover in turn 7 knows
/// what happened in turn 2.
pub struct TurnLoop<'a> {
  primary: &'a dyn ModelProvider,
  backup: Option<&'a dyn ModelProvider>,
  failover: FailoverPolicy,
  context: &'a dyn ContextPolicy,
  trace: &'a mut dyn Trace,
  tools: &'a ToolRegistry,
  session_id: SessionId,
  trace_id: TraceId,
  epochs: Vec<Epoch>,
  messages: Vec<Message>,
  message_seqs: Vec<Option<EventSeq>>,
  system: Option<String>,
  working_dir: String,
  thinking: ThinkingLevel,
  max_requests: usize,
  /// Optional boundary for coding turns that must make a named kind of
  /// progress instead of spending the request budget on inspection alone.
  progress_request_limit: Option<usize>,
  /// Explicit progress tools, or an empty list meaning every permitted
  /// mutating tool when the boundary is active.
  progress_tool_names: Vec<String>,
  progress_requests_without_progress: usize,
  progress_boundary_active: bool,
  progress_boundary_used: bool,
  /// Model requests spent by the current turn, retries and takeovers included.
  ///
  /// Counted where requests are issued rather than where rounds are driven, so the
  /// recovery loop cannot outlive the budget it is supposed to respect.
  requests: AtomicUsize,
  /// Whether model requests may advertise and execute tools.
  ///
  /// Finalization deliberately disables this capability. The normal turn loop
  /// remains tool-capable, while the bounded recovery assessment can never
  /// repeat a mutating side effect.
  tools_enabled: bool,
  interactive_tool_approval: bool,
  session_started: bool,
  /// Whether the first lifecycle event belongs to a continuation of an existing
  /// durable journal rather than a newly created session.
  resumed: bool,
  /// Interrupted tool lifecycles recovered from the canonical trace.
  interrupted_tools: Vec<rupi_core::InterruptedToolCall>,
  /// A failed reconciliation is sticky for this loop. Dropping the queue after
  /// an error would let a caller catch the error and issue a provider request
  /// against history whose side effects are still uncertain.
  recovery_blocked: bool,
  context_epoch: u32,
  /// Leading checkpoint capsule messages protected from ordinary compaction.
  checkpoint_floor: usize,
  /// First canonical sequence after the latest checkpoint barrier. This keeps
  /// later compactions from claiming that an impermeable capsule was replaced.
  checkpoint_cited_from: Option<EventSeq>,
  /// When this loop last shed model-visible history, for the policy's compaction
  /// cooldown. `None` until the first eviction; a loop that has never compacted
  /// has waited longer than any cooldown.
  last_compaction: Option<Instant>,
  /// Envelopes this loop produced. Journal positions come from the trace, and
  /// compaction needs them: the epoch names the canonical range it replaces,
  /// and a loop that cannot cite positions cites none.
  envelopes: Vec<EventEnvelope>,
  /// Journal bounds that predate this loop — a resumed session's earlier
  /// events — so a compaction after resume still names the full range it
  /// replaces. Stored as bounds, not the log itself: the loop cites history, it
  /// never replays it.
  history: Option<(rupi_core::EventSeq, rupi_core::EventSeq)>,
  compaction_strategy: CompactionStrategy,
  summarizer: Option<Summarizer>,
  checkpoint_strategy: CheckpointStrategy,
  checkpointer: Option<Checkpointer>,
}

impl<'a> TurnLoop<'a> {
  /// A loop over a primary model, a tool registry, a context policy, and a trace.
  pub fn new(
    primary: &'a dyn ModelProvider,
    tools: &'a ToolRegistry,
    context: &'a dyn ContextPolicy,
    trace: &'a mut dyn Trace,
    session_id: SessionId,
    trace_id: TraceId,
  ) -> Self {
    let capabilities = primary.capabilities();
    let epoch = Epoch {
      index: 0,
      model: primary.model().clone(),
      capabilities: capabilities.clone(),
      reason: EpochReason::Initial,
    };
    Self {
      primary,
      backup: None,
      failover: FailoverPolicy::default().requiring(capabilities),
      context,
      trace,
      tools,
      session_id,
      trace_id,
      epochs: vec![epoch],
      messages: Vec::new(),
      message_seqs: Vec::new(),
      system: None,
      working_dir: String::new(),
      thinking: ThinkingLevel::default(),
      max_requests: MAX_MODEL_REQUESTS_PER_TURN,
      progress_request_limit: None,
      progress_tool_names: Vec::new(),
      progress_requests_without_progress: 0,
      progress_boundary_active: false,
      progress_boundary_used: false,
      requests: AtomicUsize::new(0),
      tools_enabled: true,
      interactive_tool_approval: false,
      session_started: false,
      resumed: false,
      interrupted_tools: Vec::new(),
      recovery_blocked: false,
      context_epoch: 0,
      checkpoint_floor: 0,
      checkpoint_cited_from: None,
      last_compaction: None,
      envelopes: Vec::new(),
      history: None,
      compaction_strategy: CompactionStrategy::default(),
      summarizer: None,
      checkpoint_strategy: CheckpointStrategy::default(),
      checkpointer: None,
    }
  }

  /// Attach a backup model for availability-failure recovery.
  ///
  /// Capabilities are read from the provider itself rather than declared, so a
  /// failover gate can never be satisfied by a stale claim in config.
  pub fn with_backup(mut self, backup: &'a dyn ModelProvider) -> Self {
    self.failover =
      std::mem::take(&mut self.failover).with_backup(backup.model().clone(), backup.capabilities());
    self.backup = Some(backup);
    self
  }

  /// Override the failover policy, keeping the attached backup.
  ///
  /// The backup is attached separately because the caller owns the provider while
  /// the policy is what the operator tuned. A policy that names no backup keeps the
  /// attached one: dropping it here would leave a loop holding a provider it is no
  /// longer allowed to use, which reads as "failover configured, failover never
  /// happens". Detaching is explicit rather than a side effect of ordering, see
  /// [`Self::without_backup`].
  pub fn with_failover(mut self, mut policy: FailoverPolicy) -> Self {
    if policy.backup.is_none() {
      policy.backup.clone_from(&self.failover.backup);
      policy
        .backup_capabilities
        .clone_from(&self.failover.backup_capabilities);
    }
    // `required` describes the session, not the retry tuning. Replacing a policy
    // with `FailoverPolicy::default()` must not silently lower the capability gate
    // from the primary's actual requirements to the text-only baseline.
    policy.required.clone_from(&self.failover.required);
    self.failover = policy;
    self
  }

  /// Override the capability snapshot a failover policy must preserve.
  ///
  /// This is intentionally separate from [`Self::with_failover`], whose purpose
  /// is to tune retry/takeover behavior without changing what the session needs.
  pub fn with_required_capabilities(mut self, required: ModelCapabilities) -> Self {
    self.failover.required = required;
    self
  }

  /// Detach the backup: no failure may switch models.
  ///
  /// Both halves have to be cleared together. A held provider with no policy entry
  /// is inert, and a policy entry with no provider is a promise the loop cannot
  /// keep.
  pub fn without_backup(mut self) -> Self {
    self.backup = None;
    self.failover.backup = None;
    self.failover.backup_capabilities = None;
    self
  }

  /// Set the system prompt.
  pub fn with_system(mut self, system: impl Into<String>) -> Self {
    self.system = Some(system.into());
    self
  }

  /// Allow the progress surface to answer per-call mutation approval prompts.
  ///
  /// Disabled by default. The tool registry still controls configured automatic
  /// approval and allow/deny policy.
  pub fn with_interactive_tool_approval(mut self, enabled: bool) -> Self {
    self.interactive_tool_approval = enabled;
    self
  }

  /// Set the canonical workspace recorded when the session starts.
  pub fn with_working_dir(mut self, working_dir: impl Into<String>) -> Self {
    self.working_dir = working_dir.into();
    self
  }

  /// Set the configured reasoning effort for provider requests.
  pub fn with_thinking(mut self, thinking: ThinkingLevel) -> Self {
    self.thinking = thinking;
    self
  }

  /// Override the per-turn request budget.
  pub fn with_max_requests(mut self, max: usize) -> Self {
    self.max_requests = max
      .max(1)
      .min(MAX_CONFIGURED_MODEL_REQUESTS_PER_TURN as usize);
    self
  }

  /// Require a configured kind of tool progress after a bounded number of
  /// tool-bearing requests without it. While active, the next provider request
  /// exposes only the named tools; an empty name list exposes all permitted
  /// mutating tools. A successful configured progress tool satisfies the
  /// boundary for the rest of that turn. The boundary is opt-in because
  /// read-only turns are valid.
  pub fn with_progress_boundary(
    mut self,
    max_requests_without_progress: Option<usize>,
    progress_tool_names: Vec<String>,
  ) -> Self {
    self.progress_request_limit = max_requests_without_progress.filter(|limit| *limit > 0);
    self.progress_tool_names = progress_tool_names;
    self
  }

  /// Run one bounded, no-tool recovery assessment.
  ///
  /// This is intentionally a separate mode rather than a larger ordinary turn:
  /// the provider receives no tool schemas, the runtime permits one request, and
  /// the caller remains responsible for treating the resulting assessment as an
  /// incomplete recovery rather than silently converting it into success.
  pub fn run_finalization(
    &mut self,
    input: &str,
    cancel: &CancelToken,
    progress: &mut dyn TurnProgress,
  ) -> Result<TurnReport, TurnError> {
    let saved_max_requests = self.max_requests;
    let saved_tools_enabled = self.tools_enabled;
    self.max_requests = 1;
    self.tools_enabled = false;
    let result = self.run_turn(input, cancel, progress);
    self.max_requests = saved_max_requests;
    self.tools_enabled = saved_tools_enabled;
    result
  }

  /// Seed the visible history, for example after a session resume.
  pub fn with_messages(mut self, messages: Vec<Message>) -> Self {
    self.message_seqs = vec![None; messages.len()];
    self.messages = messages;
    self
  }

  /// Restore the model and context state projected by a durable session.
  ///
  /// The active epoch must name either the configured primary or attached backup;
  /// silently falling back to the primary would change both provenance and the
  /// model-visible continuation. Epoch indices are checked before any request can
  /// be sent.
  pub fn with_resume_state(mut self, state: ResumeState) -> Result<Self, TurnError> {
    if state.epochs.is_empty() {
      return Err(TurnError::Sink(
        "cannot resume a session without a model epoch".to_string(),
      ));
    }
    if state.epochs[0].index != 0
      || state
        .epochs
        .windows(2)
        .any(|epochs| epochs[1].index <= epochs[0].index)
    {
      return Err(TurnError::Sink(
        "cannot resume a session with non-monotonic model epochs".to_string(),
      ));
    }
    let active = state.epochs.last().expect("non-empty epochs");
    let active_is_primary = active.model == *self.primary.model();
    let active_is_backup = self
      .backup
      .is_some_and(|backup| active.model == *backup.model());
    if !active_is_primary && !active_is_backup {
      return Err(TurnError::Sink(format!(
        "cannot resume session: active model {} is not configured",
        active.model
      )));
    }
    self.epochs = state
      .epochs
      .into_iter()
      .map(|epoch| Epoch {
        index: epoch.index,
        model: epoch.model,
        capabilities: epoch.capabilities,
        reason: epoch.reason,
      })
      .collect();
    self.messages = state.messages;
    self.message_seqs = state.message_seqs;
    if self.message_seqs.len() != self.messages.len() {
      return Err(TurnError::Sink(
        "cannot resume a session with message sequence metadata out of alignment".into(),
      ));
    }
    if state.checkpoint_floor > self.messages.len() {
      return Err(TurnError::Sink(
        "cannot resume a session with a checkpoint floor beyond its messages".into(),
      ));
    }
    self.interrupted_tools = state.interrupted_tools;
    self.context_epoch = state.context_epoch;
    self.checkpoint_floor = state.checkpoint_floor;
    self.history = state.cited_history;
    self.checkpoint_cited_from = (self.checkpoint_floor > 0)
      .then(|| self.history.map(|(first, _)| first).unwrap_or(EventSeq(1)));
    self.resumed = true;
    Ok(self)
  }

  /// Set the compaction strategy when context policy recommends compaction.
  pub fn with_compaction_strategy(mut self, strategy: CompactionStrategy) -> Self {
    self.compaction_strategy = strategy;
    self
  }

  /// Attach a custom summarizer function, enabling summarizing compaction.
  pub fn with_summarizer(
    mut self,
    summarizer: impl Fn(&[Message]) -> String + Send + Sync + 'static,
  ) -> Self {
    self.summarizer = Some(Arc::new(summarizer));
    self.compaction_strategy = CompactionStrategy::Summarize;
    self
  }

  /// Set the checkpoint strategy when context policy suggests checkpointing.
  pub fn with_checkpoint_strategy(mut self, strategy: CheckpointStrategy) -> Self {
    self.checkpoint_strategy = strategy;
    self
  }

  /// Attach a custom checkpointer function for synthesizing context capsules.
  pub fn with_checkpointer(
    mut self,
    checkpointer: impl Fn(&[Message], &ContextState) -> ContextCapsule + Send + Sync + 'static,
  ) -> Self {
    self.checkpointer = Some(Arc::new(checkpointer));
    self.checkpoint_strategy = CheckpointStrategy::Auto;
    self
  }

  /// The model that owns generation right now.
  pub fn active_model(&self) -> ModelRef {
    self.epochs[self.epochs.len() - 1].model.clone()
  }

  /// Session identifier for this loop.
  pub fn session_id(&self) -> &SessionId {
    &self.session_id
  }

  /// List checkpoint capsules recorded for this session.
  pub fn list_checkpoints(&self) -> Result<Vec<(CheckpointId, ContextCapsule)>, TurnError> {
    self.trace.list_checkpoints().map_err(Into::into)
  }

  /// `true` if the session is currently generating with the backup model.
  pub fn failed_over(&self) -> bool {
    let active = self.active_model();
    self.backup.is_some_and(|b| b.model() == &active)
  }

  /// The backup model reference, if configured.
  pub fn backup_model(&self) -> Option<ModelRef> {
    self.backup.map(|b| b.model().clone())
  }

  /// The primary model reference.
  pub fn primary_model(&self) -> ModelRef {
    self.primary.model().clone()
  }

  /// Manually switch active generation to the configured backup model.
  pub fn failover_manual(&mut self) -> Result<ModelEpoch, TurnError> {
    self.ensure_session_started()?;
    let Some(backup) = self.backup else {
      return Err(TurnError::Refused("no backup model configured".to_string()));
    };
    if self.active_model() == *backup.model() {
      return Err(TurnError::Refused(format!(
        "backup model {} is already active",
        backup.model()
      )));
    }
    let required = self.primary.capabilities();
    let backup_caps = backup.capabilities();
    let gaps = backup_caps.gaps(&required);
    if ModelCapabilities::has_hard_gap(&gaps) {
      let gap_str = gaps
        .iter()
        .map(|g| g.to_string())
        .collect::<Vec<_>>()
        .join(", ");
      return Err(TurnError::Refused(format!(
        "failover to {} refused: hard capability shortfall ({gap_str})",
        backup.model()
      )));
    }
    let index = self
      .epoch_index()
      .checked_add(1)
      .ok_or_else(|| TurnError::Sink("model epoch space is exhausted".into()))?;
    let epoch = Epoch {
      index,
      model: backup.model().clone(),
      capabilities: backup_caps.clone(),
      reason: EpochReason::ManualSwitch,
    };
    let model_epoch = ModelEpoch {
      index: epoch.index,
      model: epoch.model.clone(),
      capabilities: epoch.capabilities.clone(),
      reason: epoch.reason.clone(),
      started_by_event: None,
    };
    self.emit(
      None,
      AgentEvent::ModelEpochStarted(ModelEpochStarted {
        epoch: epoch.index,
        model: epoch.model.clone(),
        reason: epoch.reason.clone(),
        capabilities: epoch.capabilities.clone(),
      }),
    )?;
    self.epochs.push(epoch);
    Ok(model_epoch)
  }

  /// Manually switch active generation back to the primary model.
  pub fn switch_back_manual(&mut self) -> Result<ModelEpoch, TurnError> {
    self.ensure_session_started()?;
    if self.active_model() == *self.primary.model() {
      return Err(TurnError::Refused(format!(
        "primary model {} is already active",
        self.primary.model()
      )));
    }
    let index = self
      .epoch_index()
      .checked_add(1)
      .ok_or_else(|| TurnError::Sink("model epoch space is exhausted".into()))?;
    let epoch = Epoch {
      index,
      model: self.primary.model().clone(),
      capabilities: self.primary.capabilities(),
      reason: EpochReason::ManualSwitchBack,
    };
    let model_epoch = ModelEpoch {
      index: epoch.index,
      model: epoch.model.clone(),
      capabilities: epoch.capabilities.clone(),
      reason: epoch.reason.clone(),
      started_by_event: None,
    };
    self.emit(
      None,
      AgentEvent::ModelEpochStarted(ModelEpochStarted {
        epoch: epoch.index,
        model: epoch.model.clone(),
        reason: epoch.reason.clone(),
        capabilities: epoch.capabilities.clone(),
      }),
    )?;
    self.epochs.push(epoch);
    Ok(model_epoch)
  }

  /// Reconcile an uncertain or interrupted tool call against environment state.
  pub fn reconcile_tool_call(
    &self,
    request: &rupi_core::ToolRequest,
  ) -> Result<rupi_core::ReconciliationStatus, rupi_core::ToolError> {
    self.tools.reconcile(request)
  }

  /// Reconcile tool lifecycles left open by a crashed predecessor. No provider
  /// request is permitted until every call is either normalized into a durable
  /// result or explicitly blocked for human inspection.
  fn reconcile_interrupted_tools(&mut self) -> Result<(), TurnError> {
    if self.recovery_blocked {
      return Err(TurnError::Sink(
        "cannot continue session: interrupted tool reconciliation is still unresolved".into(),
      ));
    }
    while let Some(call) = self.interrupted_tools.first().cloned() {
      let turn_id = match call.turn_id.clone() {
        Some(turn_id) => turn_id,
        None => {
          self.recovery_blocked = true;
          return Err(TurnError::Sink(format!(
            "cannot resume interrupted tool '{}': trace has no turn identity",
            call.request.name
          )));
        }
      };
      let status = match self
        .tools
        .reconcile_with_risk(&call.request, Some(call.read_only))
      {
        Ok(status) => status,
        Err(error) => {
          self.recovery_blocked = true;
          return Err(TurnError::Sink(format!(
            "cannot reconcile interrupted tool '{}': {}",
            call.request.name, error.message
          )));
        }
      };
      let (state, is_error, event, details) = match status {
        rupi_core::ReconciliationStatus::Committed { details } => (
          ToolExecutionState::Succeeded,
          false,
          AgentEvent::ToolCompleted(ToolCompleted {
            call_id: call.request.call_id.clone(),
            name: call.request.name.clone(),
            state: ToolExecutionState::Succeeded,
            duration_ms: 0,
            status: None,
            reduced: false,
            blob: None,
            visible_bytes: format!("recovered interrupted call: {details}").len() as u64,
          }),
          details,
        ),
        rupi_core::ReconciliationStatus::Unmodified { details } => (
          ToolExecutionState::Failed,
          true,
          AgentEvent::ToolFailed(ToolFailed {
            call_id: call.request.call_id.clone(),
            name: call.request.name.clone(),
            message: details.clone(),
            duration_ms: 0,
            status: None,
          }),
          details,
        ),
        rupi_core::ReconciliationStatus::Diverged { details }
        | rupi_core::ReconciliationStatus::RequiresManualInspection { details } => {
          self.recovery_blocked = true;
          return Err(TurnError::Sink(format!(
            "cannot continue session: interrupted mutating tool '{}' requires manual inspection: {details}",
            call.request.name
          )));
        }
      };
      let visible_text = format!("recovered interrupted call: {details}");
      let message = Message::new(
        Role::Tool,
        vec![ContentBlock::ToolResult(ToolResultBlock {
          id: call.request.call_id.clone(),
          name: call.request.name.clone(),
          state,
          text: visible_text,
          is_error,
          reduced: false,
        })],
      );
      let envelope = match self.emit_message_with_parent(
        Some(turn_id),
        event,
        &message,
        call
          .started_event_id
          .clone()
          .or_else(|| call.request_event_id.clone()),
      ) {
        Ok(envelope) => envelope,
        Err(error) => {
          self.recovery_blocked = true;
          return Err(error);
        }
      };
      self.push_message(message, envelope.meta.seq);
      self.interrupted_tools.remove(0);
    }
    Ok(())
  }

  fn normalize_message_seqs(&mut self) {
    match self.message_seqs.len().cmp(&self.messages.len()) {
      std::cmp::Ordering::Less => self.message_seqs.resize(self.messages.len(), None),
      std::cmp::Ordering::Greater => self.message_seqs.truncate(self.messages.len()),
      std::cmp::Ordering::Equal => {}
    }
  }

  /// Model-visible history so far.
  /// Test-visible view of the live model context.
  pub fn messages_mut(&mut self) -> &mut Vec<Message> {
    self.normalize_message_seqs();
    &mut self.messages
  }

  pub fn messages(&self) -> &[Message] {
    &self.messages
  }

  /// Run one user turn to a terminal state.
  ///
  /// A `turn_completed` event is emitted on every path, including failures: a turn
  /// with no end event leaves a session that cannot say whether it was interrupted.
  pub fn run_turn(
    &mut self,
    input: &str,
    cancel: &CancelToken,
    progress: &mut dyn TurnProgress,
  ) -> Result<TurnReport, TurnError> {
    self.run_turn_with_external_context(input, &[], cancel, progress)
  }

  /// Run one user turn with external context folded into the message path and recorded
  /// in the event trace.
  pub fn run_turn_with_external_context(
    &mut self,
    input: &str,
    external_context: &[ExternalContextItem],
    cancel: &CancelToken,
    progress: &mut dyn TurnProgress,
  ) -> Result<TurnReport, TurnError> {
    // A provider may quarantine an uncertain cancelled/idle request. This is a
    // new user turn boundary, so let it explicitly open a fresh generation;
    // automatic retries inside the previous turn never reach this hook.
    self.provider().reset_after_abandonment();
    let turn_id = TurnId::new();
    let clock = Instant::now();
    let mut report = TurnReport::new(turn_id.clone(), self.epoch_index());
    self.requests.store(0, Ordering::SeqCst);
    self.progress_requests_without_progress = 0;
    self.progress_boundary_active = false;
    self.progress_boundary_used = false;
    // Recovery may rewrite only the history that predates this turn. Keep the
    // boundary local so one turn's emergency state cannot leak into the next.
    let mut turn_history_start = self.messages.len();
    let mut overflow_recovery_used = false;

    self.ensure_session_started()?;
    let was_resumed = self.resumed;
    let restored_model = self.active_model();
    let restored_context_epoch = self.context_epoch;
    let interrupted_tools = self.interrupted_tools.len();
    self.reconcile_interrupted_tools()?;
    if was_resumed {
      self.diagnostic(
        None,
        DiagnosticLevel::Warn,
        format!(
          "resumed session: model {}, context epoch {}, reconciled {} interrupted tool(s); fresh request budget is {}",
          restored_model,
          restored_context_epoch,
          interrupted_tools,
          self.max_requests,
        ),
      )?;
      // A resumed session has one recovery boundary. Later turns are ordinary
      // turns and must not repeat the same startup notice.
      self.resumed = false;
    }

    for item in external_context {
      let bytes = item.text.len() as u64;
      let context_text = item.format_for_model();
      let msg = Message::user(context_text);
      let envelope = self.emit_message(
        Some(turn_id.clone()),
        AgentEvent::ExternalContextRetrieved(ExternalContextRetrieved {
          source: item.source.clone(),
          citation: item.citation.clone(),
          bytes,
          inline: item.inline,
          metadata: item.metadata.clone(),
        }),
        &msg,
      )?;
      self.push_message(msg, envelope.meta.seq);
    }

    let user = Message::user(input);
    let envelope = self.emit_message(
      Some(turn_id.clone()),
      AgentEvent::UserMessage(UserMessage {
        text: input.to_string(),
        attachments: 0,
      }),
      &user,
    )?;
    self.push_message(user, envelope.meta.seq);
    progress.on_user_message(input);

    // The loop is bounded by *requests*, not rounds: a turn that keeps asking for
    // tools and a turn that keeps retrying spend the same budget, because from the
    // caller's side they cost the same. Keep one request in reserve for an
    // explicit no-tool completion assessment whenever the configured budget allows
    // it; a budget of one remains a useful single ordinary request.
    let normal_request_limit = if self.max_requests > 1 {
      self.max_requests - 1
    } else {
      self.max_requests
    };
    while self.requests.load(Ordering::SeqCst) < normal_request_limit {
      if cancel.is_cancelled() {
        return self.finish(report, TurnStatus::Cancelled, clock, Some(turn_id.clone()));
      }

      let response = match self.attempt(turn_id.clone(), &mut turn_history_start, cancel, progress)
      {
        Ok(response) => response,
        // Cancellation is reported, never recovered from.
        Err(TurnFailure::Cancelled) => {
          report.requests = self.requests.load(Ordering::SeqCst);
          return self.finish(report, TurnStatus::Cancelled, clock, Some(turn_id.clone()));
        }
        Err(TurnFailure::ProviderOverflow(failure)) => {
          if overflow_recovery_used {
            self.diagnostic(
              Some(turn_id.clone()),
              DiagnosticLevel::Warn,
              "provider rejected the compacted request for context overflow; automatic recovery already used for this turn",
            )?;
            return self.finish_failure(report, failure, clock, turn_id.clone());
          }
          match self.recover_context_overflow(&turn_id, &mut turn_history_start)? {
            true => {
              overflow_recovery_used = true;
              continue;
            }
            false => return self.finish_failure(report, failure, clock, turn_id.clone()),
          }
        }
        Err(TurnFailure::Fatal(failure)) => {
          // The turn ends *before* the error is returned. A trace with no
          // `turn_completed` cannot tell a crashed session from an interrupted one,
          // and a caller that gets an error still needs the session to be coherent.
          report.requests = self.requests.load(Ordering::SeqCst);
          return self.finish_failure(report, failure, clock, turn_id.clone());
        }
        Err(TurnFailure::Sink(error)) => return Err(TurnError::from(error)),
      };
      let RecordedResponse {
        assistant_event_id,
        calls,
        rejected_calls,
      } = self.record_response(response, &mut report)?;

      if calls.is_empty() {
        // The model answered instead of asking: the turn is over.
        return self.finish(report, TurnStatus::Completed, clock, Some(turn_id.clone()));
      }

      if !self.tools_enabled && !calls.is_empty() {
        self.record_unexecuted_calls(
          turn_id.clone(),
          assistant_event_id,
          &calls,
          progress,
          "finalization mode does not execute tools",
        )?;
        report.budget_exhausted = true;
        self.diagnostic(
          Some(turn_id.clone()),
          DiagnosticLevel::Warn,
          "finalization received a tool request; no tool was executed",
        )?;
        return self.finish(
          report,
          TurnStatus::BudgetExhausted,
          clock,
          Some(turn_id.clone()),
        );
      }

      let tool_calls = u32::try_from(calls.len())
        .map_err(|_| TurnError::Sink("tool-call count exceeds durable limit".into()))?;
      report.tool_calls = report
        .tool_calls
        .checked_add(tool_calls)
        .ok_or_else(|| TurnError::Sink("turn tool-call count is exhausted".into()))?;
      let progress_succeeded = self.execute_calls(
        turn_id.clone(),
        assistant_event_id,
        &calls,
        &rejected_calls,
        cancel,
        progress,
      )?;
      self.observe_progress(&turn_id, progress_succeeded)?;
    }

    if self.max_requests > 1 && self.requests.load(Ordering::SeqCst) < self.max_requests {
      if cancel.is_cancelled() {
        report.requests = self.requests.load(Ordering::SeqCst);
        return self.finish(report, TurnStatus::Cancelled, clock, Some(turn_id.clone()));
      }

      self.append_finalization_instruction(&turn_id)?;
      let finalization = {
        let tools_enabled = self.tools_enabled;
        self.tools_enabled = false;
        let result = self.attempt(turn_id.clone(), &mut turn_history_start, cancel, progress);
        self.tools_enabled = tools_enabled;
        result
      };
      let response = match finalization {
        Ok(response) => response,
        Err(TurnFailure::Cancelled) => {
          report.requests = self.requests.load(Ordering::SeqCst);
          return self.finish(report, TurnStatus::Cancelled, clock, Some(turn_id.clone()));
        }
        Err(TurnFailure::ProviderOverflow(failure) | TurnFailure::Fatal(failure)) => {
          report.requests = self.requests.load(Ordering::SeqCst);
          return self.finish_failure(report, failure, clock, turn_id.clone());
        }
        Err(TurnFailure::Sink(error)) => return Err(TurnError::from(error)),
      };
      let RecordedResponse {
        assistant_event_id,
        calls,
        ..
      } = self.record_response(response, &mut report)?;
      report.budget_exhausted = true;
      if calls.is_empty() {
        self.diagnostic(
          Some(turn_id.clone()),
          DiagnosticLevel::Warn,
          "request budget exhausted; finalization answer is incomplete and the session can be resumed",
        )?;
      } else {
        let tool_calls = u32::try_from(calls.len()).map_err(|_| {
          TurnError::Sink("finalization tool-call count exceeds durable limit".into())
        })?;
        report.tool_calls = report
          .tool_calls
          .checked_add(tool_calls)
          .ok_or_else(|| TurnError::Sink("turn tool-call count is exhausted".into()))?;
        self.record_unexecuted_calls(
          turn_id.clone(),
          assistant_event_id,
          &calls,
          progress,
          "request-budget finalization does not execute tools",
        )?;
        self.diagnostic(
          Some(turn_id.clone()),
          DiagnosticLevel::Warn,
          "request budget exhausted; finalization requested tools and none were executed, so the session can be resumed",
        )?;
      }
      return self.finish(
        report,
        TurnStatus::BudgetExhausted,
        clock,
        Some(turn_id.clone()),
      );
    }

    // Out of requests, not out of options: the distinction belongs in the trace.
    report.budget_exhausted = true;
    self.diagnostic(
      Some(turn_id.clone()),
      DiagnosticLevel::Warn,
      format!(
        "request budget exhausted after {} model requests without a final answer; the session can be resumed",
        self.max_requests
      ),
    )?;
    self.finish(
      report,
      TurnStatus::BudgetExhausted,
      clock,
      Some(turn_id.clone()),
    )
  }

  /// Close the session explicitly.
  ///
  /// An explicit close is what distinguishes "the user finished" from "the process
  /// died", and that distinction is the only thing a resume can trust.
  pub fn end_session(&mut self, reason: SessionEndReason) -> Result<(), TurnError> {
    self.emit(None, AgentEvent::SessionEnded(SessionEnded { reason }))?;
    self.trace.flush()?;
    Ok(())
  }

  /// Emit a diagnostic that is not part of a turn.
  pub fn note(
    &mut self,
    level: DiagnosticLevel,
    message: impl Into<String>,
  ) -> Result<(), TurnError> {
    self.diagnostic(None, level, message)
  }

  fn epoch_index(&self) -> u32 {
    self.epochs[self.epochs.len() - 1].index
  }

  /// Open the session once, under the epoch that owns it.
  fn ensure_session_started(&mut self) -> Result<(), TurnError> {
    if self.session_started {
      return Ok(());
    }
    self.session_started = true;
    let epoch = if self.resumed {
      self.epochs[self.epochs.len() - 1].clone()
    } else {
      self.epochs[0].clone()
    };
    self.emit(
      None,
      AgentEvent::SessionStarted(SessionStarted {
        working_dir: self.working_dir.clone(),
        model: epoch.model.clone(),
        capabilities: epoch.capabilities.clone(),
        resumed: self.resumed,
      }),
    )?;
    if self.resumed {
      // Prior epochs already exist in the durable journal. Re-emitting epoch 0
      // would create a duplicate identity and make the resumed timeline appear
      // to move backwards.
      return Ok(());
    }
    self
      .emit(
        None,
        AgentEvent::ModelEpochStarted(ModelEpochStarted {
          epoch: epoch.index,
          model: epoch.model,
          reason: epoch.reason,
          capabilities: epoch.capabilities,
        }),
      )
      .map(|_| ())
  }
}

impl<'a> TurnLoop<'a> {
  fn emit(
    &mut self,
    turn_id: Option<TurnId>,
    event: AgentEvent,
  ) -> Result<EventEnvelope, TurnError> {
    self.emit_with_sink(turn_id, event, false)
  }

  fn new_envelope(&self, turn_id: Option<TurnId>, event: AgentEvent) -> EventEnvelope {
    self.new_envelope_with_parent(turn_id, event, None)
  }

  fn new_envelope_with_parent(
    &self,
    turn_id: Option<TurnId>,
    event: AgentEvent,
    parent_event_id: Option<rupi_core::EventId>,
  ) -> EventEnvelope {
    let epoch = &self.epochs[self.epochs.len() - 1];
    let mut meta = EventMeta::new(self.session_id.clone(), self.trace_id.clone());
    meta.model_epoch = Some(epoch.index);
    meta.model = Some(epoch.model.clone());
    meta.parent_event_id = parent_event_id;
    if let Some(turn_id) = turn_id {
      meta.turn_id = Some(turn_id);
    }
    EventEnvelope::new(meta, event)
  }

  fn emit_with_parent(
    &mut self,
    turn_id: Option<TurnId>,
    event: AgentEvent,
    parent_event_id: Option<rupi_core::EventId>,
  ) -> Result<EventEnvelope, TurnError> {
    self.emit_with_sink_parent(turn_id, event, false, parent_event_id)
  }

  fn emit_with_sink(
    &mut self,
    turn_id: Option<TurnId>,
    event: AgentEvent,
    without_message: bool,
  ) -> Result<EventEnvelope, TurnError> {
    self.emit_with_sink_parent(turn_id, event, without_message, None)
  }

  fn emit_with_sink_parent(
    &mut self,
    turn_id: Option<TurnId>,
    event: AgentEvent,
    without_message: bool,
    parent_event_id: Option<rupi_core::EventId>,
  ) -> Result<EventEnvelope, TurnError> {
    let mut envelope = self.new_envelope_with_parent(turn_id, event, parent_event_id);
    if without_message {
      self.trace.emit_without_message(&mut envelope)?;
    } else {
      self.trace.emit(&mut envelope)?;
    }
    self.envelopes.push(envelope.clone());
    Ok(envelope)
  }

  /// The oldest journal position this loop can cite.
  ///
  /// A compaction replaces the ordinary model-visible prefix, whether those
  /// records were written by this process or restored before it started. After
  /// a checkpoint, the protected capsule establishes a newer citation floor;
  /// `EventSeq(0)` remains the marker for a purely in-memory loop.
  fn first_cited_seq(&self) -> rupi_core::EventSeq {
    self
      .checkpoint_cited_from
      .or_else(|| self.history.map(|(first, _)| first))
      .or_else(|| {
        self
          .envelopes
          .iter()
          .find_map(|envelope| envelope.meta.seq)
          .map(|seq| {
            if seq.0 == 1 {
              seq
            } else {
              rupi_core::EventSeq(0)
            }
          })
      })
      .unwrap_or(rupi_core::EventSeq(0))
  }

  /// Declare the journal bounds of history this loop inherited but did not emit.
  ///
  /// A resumed loop is handed messages, not envelopes; without this, a
  /// compaction after resume would name a range that silently omits everything
  /// the previous run wrote.
  pub fn with_cited_history(
    mut self,
    first: rupi_core::EventSeq,
    last: rupi_core::EventSeq,
  ) -> Self {
    self.history = Some((first, last));
    self
  }

  fn emit_message(
    &mut self,
    turn_id: Option<TurnId>,
    event: AgentEvent,
    message: &Message,
  ) -> Result<EventEnvelope, TurnError> {
    self.emit_message_with_parent(turn_id, event, message, None)
  }

  fn emit_message_with_parent(
    &mut self,
    turn_id: Option<TurnId>,
    event: AgentEvent,
    message: &Message,
    parent_event_id: Option<rupi_core::EventId>,
  ) -> Result<EventEnvelope, TurnError> {
    let mut envelope = self.new_envelope_with_parent(turn_id, event, parent_event_id);
    self.trace.emit_message(&mut envelope, message)?;
    self.envelopes.push(envelope.clone());
    Ok(envelope)
  }

  fn push_message(&mut self, message: Message, seq: Option<EventSeq>) {
    self.messages.push(message);
    self.message_seqs.push(seq);
  }

  fn diagnostic(
    &mut self,
    turn_id: Option<TurnId>,
    level: DiagnosticLevel,
    message: impl Into<String>,
  ) -> Result<(), TurnError> {
    self
      .emit(
        turn_id,
        AgentEvent::Diagnostic(Diagnostic {
          level,
          message: message.into(),
        }),
      )
      .map(|_| ())
  }

  fn finish(
    &mut self,
    mut report: TurnReport,
    status: TurnStatus,
    clock: Instant,
    turn_id: Option<TurnId>,
  ) -> Result<TurnReport, TurnError> {
    report.status = status.clone();
    report.duration_ms = elapsed_ms(clock);
    self.emit(
      turn_id.clone(),
      AgentEvent::TurnCompleted(TurnCompleted {
        status,
        duration_ms: report.duration_ms,
      }),
    )?;
    // The turn boundary is the durable checkpoint: everything the next model needs
    // must be on disk before this returns.
    self.trace.flush()?;
    Ok(report)
  }

  /// Close a failed turn before returning the provider's original failure.
  fn finish_failure(
    &mut self,
    mut report: TurnReport,
    failure: ModelFailure,
    clock: Instant,
    turn_id: TurnId,
  ) -> Result<TurnReport, TurnError> {
    report.requests = self.requests.load(Ordering::SeqCst);
    let status = TurnStatus::Failed { kind: failure.kind };
    let _ = self.finish(report, status, clock, Some(turn_id))?;
    Err(TurnError::Unavailable(failure))
  }

  /// Prepare one bounded local recovery candidate after an uncommitted provider
  /// overflow. The live message vector is not changed until the candidate has
  /// been assembled with the same request shape used for production requests.
  fn recover_context_overflow(
    &mut self,
    turn_id: &TurnId,
    turn_history_start: &mut usize,
  ) -> Result<bool, TurnError> {
    let prefix_end = (*turn_history_start).min(self.messages.len());
    let protected = self.checkpoint_floor.min(prefix_end);
    if prefix_end <= protected {
      self.diagnostic(
        Some(turn_id.clone()),
        DiagnosticLevel::Warn,
        "provider rejected the request for context overflow; current-turn content alone cannot be compacted safely",
      )?;
      return Ok(false);
    }
    if self.requests.load(Ordering::SeqCst) >= self.max_requests {
      self.diagnostic(
        Some(turn_id.clone()),
        DiagnosticLevel::Warn,
        "provider rejected the request for context overflow; request budget cannot pay for a reissue",
      )?;
      return Ok(false);
    }

    let target = overflow_recovery_target(self.provider().capabilities().context_window);
    // A proactive structural reduction may already have made the exact live
    // request fit. Reuse that canonical state instead of opening a redundant
    // emergency epoch; the normal builder will issue the same request again.
    if self.turn_has_structural_compaction(turn_id) {
      let request = self.assemble_request(self.messages.clone());
      if estimate_tokens(&request) <= target {
        self.diagnostic(
          Some(turn_id.clone()),
          DiagnosticLevel::Info,
          "provider rejected the request for context overflow; existing compaction already bounded the context, retrying once",
        )?;
        return Ok(true);
      }
    }

    let prefix = self.messages[protected..prefix_end].to_vec();
    let protected_messages = self.messages[..protected].to_vec();
    let suffix = self.messages[prefix_end..].to_vec();
    let source = match &self.summarizer {
      Some(summarizer) => summarizer(&prefix),
      None => structured_summary(&prefix),
    };

    // The summary is reduced by character-boundary-safe steps. A bounded number
    // of attempts keeps a pathological local summarizer from consuming the turn,
    // while still reaching an empty-summary candidate for very large histories.
    let mut summary_bytes = source.len();
    let mut accepted = None;
    for _ in 0..=64 {
      let summary = truncate_utf8_to_bytes(&source, summary_bytes).to_string();
      let mut candidate_messages = Vec::with_capacity(protected_messages.len() + suffix.len() + 1);
      candidate_messages.extend(protected_messages.iter().cloned());
      candidate_messages.push(Message::user(summary.clone()));
      candidate_messages.extend(suffix.iter().cloned());
      let request = self.assemble_request(candidate_messages);
      if estimate_tokens(&request) <= target {
        accepted = Some(summary);
        break;
      }
      if summary_bytes == 0 {
        break;
      }
      let next = summary_bytes / 2;
      let next = truncate_utf8_to_bytes(&source, next).len();
      if next == summary_bytes {
        summary_bytes = summary_bytes.saturating_sub(1);
      } else {
        summary_bytes = next;
      }
    }

    let Some(summary) = accepted else {
      self.diagnostic(
        Some(turn_id.clone()),
        DiagnosticLevel::Warn,
        "provider rejected the request for context overflow; no bounded compacted request fits the active context window",
      )?;
      return Ok(false);
    };

    self.diagnostic(
      Some(turn_id.clone()),
      DiagnosticLevel::Info,
      "provider rejected the request for context overflow; compacting prior history and retrying once",
    )?;
    let replaced = self.compact_prefix(turn_id, prefix_end, &summary)?;
    if replaced == 0 {
      return Ok(false);
    }
    // The ordinary prefix was replaced by exactly one summary message. The
    // checkpoint floor and the captured current-turn tail remain verbatim.
    *turn_history_start = self.checkpoint_floor.saturating_add(1);
    debug_assert_eq!(
      self.messages.get(self.checkpoint_floor.saturating_add(1)..),
      Some(suffix.as_slice())
    );
    Ok(true)
  }

  fn turn_has_structural_compaction(&self, turn_id: &TurnId) -> bool {
    self.envelopes.iter().any(|envelope| {
      envelope.meta.turn_id.as_ref() == Some(turn_id)
        && matches!(
          &envelope.event,
          AgentEvent::ContextCompactionCompleted(completed)
            if completed.level.requires_safe_boundary()
        )
    })
  }

  /// One model request, with retries and at most one takeover.
  ///
  /// The loop may re-issue only while nothing has been streamed. That single rule
  /// is what keeps recovery from duplicating committed content.
  fn attempt(
    &mut self,
    turn_id: TurnId,
    turn_history_start: &mut usize,
    cancel: &CancelToken,
    progress: &mut dyn TurnProgress,
  ) -> Result<Response, TurnFailure> {
    // Requests are counted against the *active* model: a takeover is not another
    // attempt against a model that is already down, it is the first attempt against
    // a model that may still work.
    let mut attempts_on_model: u32 = 0;
    let mut epoch_of_attempts = self.epoch_index();
    loop {
      if self.epoch_index() != epoch_of_attempts {
        epoch_of_attempts = self.epoch_index();
        attempts_on_model = 0;
      }
      attempts_on_model = attempts_on_model
        .checked_add(1)
        .ok_or_else(|| TurnFailure::Sink(SinkError("model attempt count is exhausted".into())))?;
      let request = self
        .build_request(&turn_id, turn_history_start)
        .map_err(TurnFailure::from)?;
      // The turn's request budget is spent here, at the point the request exists.
      let requests = self.requests.load(Ordering::SeqCst);
      if requests == usize::MAX {
        return Err(TurnFailure::Sink(SinkError(
          "model request count is exhausted".into(),
        )));
      }
      self.requests.fetch_add(1, Ordering::SeqCst);
      let epoch = self.epoch_index();
      let model = self.active_model();
      let estimate = estimate_tokens(&request);

      self
        .emit(
          Some(turn_id.clone()),
          AgentEvent::ModelRequestStarted(ModelRequestStarted {
            epoch,
            model: model.clone(),
            message_count: u32::try_from(request.messages.len()).map_err(|_| {
              TurnFailure::Sink(SinkError(
                "model message count exceeds durable limit".into(),
              ))
            })?,
            context_tokens_est: estimate,
            tools_exposed: u32::try_from(request.tools.len()).map_err(|_| {
              TurnFailure::Sink(SinkError("tool-spec count exceeds durable limit".into()))
            })?,
          }),
        )
        .map_err(TurnFailure::from)?;
      let request_number = self.requests.load(Ordering::SeqCst);
      progress.on_request_started_with_budget(&model, request_number, self.max_requests);

      let clock = Instant::now();
      let attribution = StreamAttribution {
        turn_id: turn_id.clone(),
        session_id: self.session_id.clone(),
        trace_id: self.trace_id.clone(),
        epoch,
        model: model.clone(),
      };
      let provider = self.provider();
      let mut collector = Collector::new(
        progress,
        &mut *self.trace,
        attribution,
        cancel.clone(),
        clock,
      );
      let outcome = provider.stream(&request, &mut collector, cancel);
      let duration_ms = elapsed_ms(clock);
      let Collector {
        text,
        calls,
        rejected_calls,
        committed,
        reasoning_provenance: provenance,
        sink_error,
        first_delta_ms,
        ..
      } = collector;
      if let Some(error) = sink_error {
        return Err(TurnFailure::Sink(error));
      }

      let mut failed_usage = None;
      let failure = match outcome {
        // Transport said done. Whether the *model* finished is a separate
        // question, and answering it wrongly is how a runtime accepts a half
        // answer as final.
        Ok(usage) => {
          // "Produced" means *content the user would see*. Tool calls alone do not
          // make an unfinished response a truncation of an answer.
          let produced = !text.is_empty();
          match completion_failure(&usage, produced, committed) {
            Some(failure) => {
              // Keep provider completion evidence on the request boundary even
              // when the answer is unusable. In particular, `length` must not
              // disappear into a generic failed-request event: it tells the
              // operator that the output budget, not the transport, stopped the
              // model.
              failed_usage = Some(usage);
              failure
            }
            None => {
              let tool_calls = u32::try_from(calls.len()).map_err(|_| {
                TurnFailure::Sink(SinkError("tool-call count exceeds durable limit".into()))
              })?;
              let completion = self.new_envelope(
                Some(turn_id.clone()),
                AgentEvent::ModelRequestCompleted(ModelRequestCompleted {
                  epoch,
                  model: model.clone(),
                  finish_reason: usage.finish_reason.clone().or_else(|| {
                    usage.is_certain().then(|| {
                      if calls.is_empty() {
                        "stop"
                      } else {
                        "tool_calls"
                      }
                      .into()
                    })
                  }),
                  input_tokens: usage.input_tokens,
                  uncached_input_tokens: usage.uncached_input_tokens,
                  logical_prompt_tokens: usage.logical_prompt_tokens,
                  cache_read_tokens: usage.cache_read_tokens,
                  cache_write_tokens: usage.cache_write_tokens,
                  output_tokens: usage.output_tokens,
                  provider_total_tokens: usage.provider_total_tokens,
                  duration_ms,
                  tool_calls,
                  reasoning_provenance: provenance,
                  first_delta_ms,
                }),
              );
              return Ok(Response {
                epoch,
                text: (!text.is_empty()).then_some(text),
                calls,
                rejected_calls,
                completion,
              });
            }
          }
        }
        Err(failure) => {
          // Output reached the user the moment it was emitted, so it is recorded on
          // the failure rather than inferred afterwards.
          let mut failure = failure;
          failure.partial_output_emitted |= committed;
          failure
        }
      };

      let mut failure = failure;
      failure.attempts = attempts_on_model;
      failure.model = Some(model.clone());
      // The request consumed its span either way; close it so the trace never shows
      // a request that never ended.
      let failed_completion = self
        .emit(
          Some(turn_id.clone()),
          AgentEvent::ModelRequestCompleted(ModelRequestCompleted {
            epoch,
            model: model.clone(),
            finish_reason: failed_usage
              .as_ref()
              .and_then(|usage| usage.finish_reason.clone()),
            input_tokens: failed_usage.as_ref().and_then(|usage| usage.input_tokens),
            uncached_input_tokens: failed_usage
              .as_ref()
              .and_then(|usage| usage.uncached_input_tokens),
            logical_prompt_tokens: failed_usage
              .as_ref()
              .and_then(|usage| usage.logical_prompt_tokens),
            cache_read_tokens: failed_usage
              .as_ref()
              .and_then(|usage| usage.cache_read_tokens),
            cache_write_tokens: failed_usage
              .as_ref()
              .and_then(|usage| usage.cache_write_tokens),
            output_tokens: failed_usage.as_ref().and_then(|usage| usage.output_tokens),
            provider_total_tokens: failed_usage
              .as_ref()
              .and_then(|usage| usage.provider_total_tokens),
            duration_ms,
            tool_calls: u32::try_from(calls.len()).map_err(|_| {
              TurnFailure::Sink(SinkError("tool-call count exceeds durable limit".into()))
            })?,
            reasoning_provenance: provenance,
            first_delta_ms,
          }),
        )
        .map_err(TurnFailure::from)?;
      self
        .record_unexecuted_calls(
          turn_id.clone(),
          failed_completion.meta.event_id,
          &calls,
          progress,
          "model response did not complete; tool was not executed",
        )
        .map_err(TurnFailure::from)?;
      self
        .diagnostic(
          Some(turn_id.clone()),
          DiagnosticLevel::Warn,
          format!(
            "model request failed ({}): {}",
            failure.kind, failure.message
          ),
        )
        .map_err(TurnFailure::from)?;

      if cancel.is_cancelled() {
        // The model stopped responding because the user asked it to. Reporting that
        // as an outage would be wrong twice over: it is not a fault, and recovery
        // must not run.
        return Err(TurnFailure::Cancelled);
      }
      // Provider overflow is a distinct outcome. It is eligible for the outer
      // turn's one-shot local compaction only when this request committed no
      // reasoning, text, or decoded tool call. It must never enter failover.
      if failure.kind == ModelFailureKind::ContextOverflow {
        if failure.partial_output_emitted {
          return Err(TurnFailure::Fatal(failure));
        }
        return Err(TurnFailure::ProviderOverflow(failure));
      }
      // The request budget covers retries and takeovers as well as ordinary
      // tool rounds. Do not let the inner recovery loop spend a second request
      // after the outer turn budget has already been consumed; doing so turns a
      // configured one-request stopgate into a misleading quarantine failure.
      if self.requests.load(Ordering::SeqCst) >= self.max_requests {
        return Err(TurnFailure::Fatal(failure));
      }
      match self
        .recover(turn_id.clone(), &failure, turn_history_start, cancel)
        .map_err(TurnFailure::from)?
      {
        Action::Retry => {
          let delay_ms = failure.retry_after_ms.unwrap_or_else(|| {
            let exp = 100u64.saturating_mul(1u64 << (attempts_on_model.saturating_sub(1).min(5)));
            exp.min(2_000)
          });
          if !Self::sleep_with_cancel(Duration::from_millis(delay_ms), cancel) {
            return Err(TurnFailure::Cancelled);
          }
          continue;
        }
        Action::Takeover => continue,
        Action::Stop => return Err(TurnFailure::Fatal(failure)),
      }
    }
  }

  /// Sleep for the given duration while respecting cancellation.
  fn sleep_with_cancel(duration: Duration, cancel: &CancelToken) -> bool {
    if cancel.is_cancelled() {
      return false;
    }
    let start = Instant::now();
    while start.elapsed() < duration {
      if cancel.is_cancelled() {
        return false;
      }
      let remaining = duration.saturating_sub(start.elapsed());
      let step = remaining.min(Duration::from_millis(20));
      std::thread::sleep(step);
    }
    !cancel.is_cancelled()
  }

  /// The provider for the active epoch.
  ///
  /// Matches the provider with the active model reference. Once switched,
  /// the active model remains until explicitly changed by the user or
  /// automatically transitioned by recovery.
  fn provider(&self) -> &'a dyn ModelProvider {
    let active = self.active_model();
    if let Some(backup) = self.backup {
      if backup.model() == &active {
        return backup;
      }
    }
    self.primary
  }

  /// Apply the failover policy's decision to a failure.
  fn recover(
    &mut self,
    turn_id: TurnId,
    failure: &ModelFailure,
    turn_history_start: &mut usize,
    cancel: &CancelToken,
  ) -> Result<Action, TurnError> {
    if cancel.is_cancelled() || matches!(failure.kind, ModelFailureKind::Cancelled) {
      return Ok(Action::Stop);
    }
    let decision = self.failover.decide(
      failure.kind,
      failure.attempts,
      failure.partial_output_emitted,
    );
    match decision {
      Recovery::Retry {
        attempt: which,
        max_attempts,
      } => {
        self.emit(
          Some(turn_id.clone()),
          AgentEvent::ModelRetry(ModelRetry {
            attempt: which + 1,
            max_attempts,
            kind: failure.kind,
            retry_after_ms: failure.retry_after_ms,
            will_failover: self.backup.is_some() && which + 1 >= max_attempts,
          }),
        )?;
        Ok(Action::Retry)
      }
      Recovery::Failover { to, gaps } => {
        let Some(backup) = self.backup else {
          return Ok(Action::Stop);
        };
        if backup.model() != &to {
          // The policy named a model this loop never validated. Switching to it
          // would be an unannounced second primary.
          self.diagnostic(
            Some(turn_id.clone()),
            DiagnosticLevel::Error,
            format!("failover to {to} refused: it is not the attached backup"),
          )?;
          return Ok(Action::Stop);
        }
        if self.active_model() == to {
          // The policy is still pointing at the model already answering, which
          // happens once the backup itself fails. Another epoch for the same model
          // is ping-pong wearing a takeover label: it burns the request budget,
          // records transitions that changed nothing, and hides that the only
          // remaining option was to stop.
          self.diagnostic(
            Some(turn_id.clone()),
            DiagnosticLevel::Error,
            format!("failover to {to} refused: it is the active model"),
          )?;
          return Ok(Action::Stop);
        }
        let narrow = gaps
          .iter()
          .any(|gap| matches!(gap, CapabilityGap::ContextWindow { .. }));
        let dropped = if narrow {
          self.rebudget(backup, turn_id.clone(), turn_history_start)?
        } else {
          0
        };
        let from = self.active_model();
        let index = self
          .epoch_index()
          .checked_add(1)
          .ok_or_else(|| TurnError::Sink("model epoch space is exhausted".into()))?;
        let epoch = Epoch {
          index,
          model: to.clone(),
          capabilities: backup.capabilities(),
          reason: EpochReason::AutomaticFailover,
        };
        self.emit(
          Some(turn_id.clone()),
          AgentEvent::ModelFailover(ModelFailover {
            from,
            to: to.clone(),
            kind: failure.kind,
            gaps,
            // Whether history was actually shortened, not whether the backup's window
            // is smaller: with one turn in flight there is nothing to drop, and a
            // recorded reduction that never happened is exactly the false provenance
            // this codebase refuses elsewhere.
            compacted: dropped > 0,
          }),
        )?;
        self.emit(
          Some(turn_id.clone()),
          AgentEvent::ModelEpochStarted(ModelEpochStarted {
            epoch: epoch.index,
            model: epoch.model.clone(),
            reason: epoch.reason.clone(),
            capabilities: epoch.capabilities.clone(),
          }),
        )?;
        self.epochs.push(epoch);
        Ok(Action::Takeover)
      }
      Recovery::Refused { to, gaps } => {
        let gaps = gaps
          .iter()
          .map(|gap| gap.to_string())
          .collect::<Vec<_>>()
          .join(", ");
        self.diagnostic(
          Some(turn_id.clone()),
          DiagnosticLevel::Error,
          format!("failover to {to} refused: the backup cannot do this work ({gaps})"),
        )?;
        Ok(Action::Stop)
      }
      Recovery::Abort => Ok(Action::Stop),
    }
  }
}

impl<'a> TurnLoop<'a> {
  /// Shrink the model-visible window before takeover into a smaller context.
  ///
  /// This is not summarizing compaction: that needs a model, and the model in hand
  /// is the one that is failing. Oldest turns are dropped and the fact is
  /// recorded, because a silently shortened history is indistinguishable from a
  /// lost one.
  ///
  /// Returns how many turns were dropped, which is the difference between reporting
  /// a rebudget and performing one.
  fn rebudget(
    &mut self,
    backup: &'a dyn ModelProvider,
    turn_id: TurnId,
    turn_history_start: &mut usize,
  ) -> Result<u32, TurnError> {
    let target = overflow_recovery_target(backup.capabilities().context_window);
    let dropped = self.evict_oldest_for(backup, target, &turn_id, turn_history_start)?;
    // Validate the exact backup request, including its system guidance and the
    // tools that its capabilities and the current execution policy will expose.
    // This also refuses a request that cannot fit even when there is no
    // evictable history; switching epochs must not knowingly send an oversized
    // prompt.
    if self.estimate_request_for(backup, self.messages.clone()) > target {
      return Err(TurnError::Sink(format!(
        "cannot safely rebudget the complete backup request below its context target of {target} tokens"
      )));
    }
    Ok(dropped)
  }

  /// Drop the oldest model-visible turns until the estimate reaches `target`, and
  /// record the fact with a recovery reference.
  ///
  /// This is the runtime's own compaction tier: no model, no summary, nothing
  /// outside the model-visible vector. The canonical trace is untouched — it holds
  /// every dropped turn — and the event says what left the window and where the
  /// proof lives. The newest turn is never dropped: without it there is nothing to
  /// continue.
  fn evict_oldest(
    &mut self,
    target: u64,
    turn_id: &TurnId,
    turn_history_start: &mut usize,
  ) -> Result<u32, TurnError> {
    let provider = self.provider();
    self.evict_oldest_for(provider, target, turn_id, turn_history_start)
  }

  fn evict_oldest_for(
    &mut self,
    provider: &dyn ModelProvider,
    target: u64,
    turn_id: &TurnId,
    turn_history_start: &mut usize,
  ) -> Result<u32, TurnError> {
    self.normalize_message_seqs();
    let before = self.estimate_request_for(provider, self.messages.clone());
    let turn_start = (*turn_history_start).min(self.messages.len());
    // The current-turn suffix and a restored checkpoint capsule are both
    // non-evictable. The latter is a durable barrier, not ordinary history.
    let checkpoint = self.checkpoint_floor.min(self.messages.len());
    let evictable_start = checkpoint;
    let evictable_end = turn_start.max(checkpoint).min(self.messages.len());
    let max_drop = evictable_end.saturating_sub(evictable_start);
    let mut desired = 0usize;
    while desired < max_drop
      && self.estimate_request_after_eviction(provider, evictable_start, desired) > target
    {
      desired += 1;
    }
    // A reduction may remove only complete conversation turns. If the byte
    // target lands inside an assistant tool-call/result pair, retain the last
    // safe boundary instead of handing a provider an invalid protocol history.
    let dropped = (0..=desired)
      .rev()
      .find(|drop| {
        safe_eviction_boundary(
          &self.messages,
          evictable_start,
          evictable_start + *drop,
          evictable_end,
        )
      })
      .unwrap_or(0);
    if dropped > 0 {
      let visible = self.estimate_request_after_eviction(provider, evictable_start, dropped);
      let dropped_count = u32::try_from(dropped)
        .map_err(|_| TurnError::Sink("eviction message count exceeds durable limit".into()))?;
      let retained_count = u32::try_from(self.messages.len().saturating_sub(dropped))
        .map_err(|_| TurnError::Sink("retained message count exceeds durable limit".into()))?;
      let dropped_messages =
        serde_json::to_vec(&self.messages[evictable_start..evictable_start + dropped])
          .map_err(|error| TurnError::Sink(format!("cannot serialize reduced history: {error}")))?;
      let blob = self.trace.put_payload(&dropped_messages)?;
      let recovery_ref = blob.as_ref().map(BlobRef::recovery_ref);
      self.emit(
        Some(turn_id.clone()),
        AgentEvent::ContextReduced(ContextReduced {
          reason: ReductionReason::RecentTargetExceeded {
            target_tokens: target,
          },
          original_bytes: before,
          visible_bytes: visible,
          removed_messages: dropped_count,
          retained_messages: retained_count,
          recovery_ref,
          blob,
          tool_call_id: None,
        }),
      )?;
      self
        .messages
        .drain(evictable_start..evictable_start + dropped);
      self
        .message_seqs
        .drain(evictable_start..evictable_start + dropped);
      if *turn_history_start > evictable_start {
        *turn_history_start = (*turn_history_start).saturating_sub(dropped);
      }
      // L0 eviction is payload/history reduction, not a compaction epoch. The
      // durable context epoch advances only when an L1/L2 summary or L3
      // checkpoint establishes a new semantic window.
      self.last_compaction = Some(Instant::now());
      return Ok(dropped_count);
    }
    Ok(0)
  }

  /// Replace the oldest model-visible messages with a summary at this safe
  /// boundary, opening a durable compaction epoch.
  ///
  /// This is the summarizing tier of the reduction ladder: eviction loses whole
  /// turns, a summary keeps their substance. The caller owns what the summary
  /// says — typically a model-written condensation obtained through the same
  /// provider — because deciding what to keep is a judgment the loop does not
  /// make. What the loop guarantees is the bookkeeping:
  ///
  /// - the summary enters canonical history as a message, so the canonical trace
  ///   still holds every word and a reader of the session log finds what replaced
  ///   the window;
  /// - a `ContextCompactionEpoch` opens in the journal, naming the summary
  ///   payload and the inclusive range of journal positions it replaces, so
  ///   "the model saw a summary" is auditable and reversible in principle;
  /// - the live context becomes the retained tail plus the summary (while a
  ///   restored checkpoint capsule remains at the protected front).
  ///
  /// The session projection records the same transition so resume can restore
  /// the reduced window without replaying or silently re-expanding history.
  pub fn compact(
    &mut self,
    turn_id: &TurnId,
    summary: &str,
    retained: usize,
  ) -> Result<u32, TurnError> {
    // `retained` counts messages that survive besides the summary. The summary
    // is inserted before that untouched suffix so chronological context remains
    // [summary of older history, retained history].
    let kept = if self.messages.is_empty() {
      0
    } else {
      retained.min(self.messages.len().saturating_sub(1))
    };
    let removed = self.messages.len().saturating_sub(kept);
    self.compact_range(
      turn_id,
      removed,
      summary,
      ContextLevel::L1Ordinary,
      format!("summarizing {removed} oldest messages"),
    )
  }

  /// Replace exactly the oldest `prefix_end` messages with one durable summary.
  ///
  /// The untouched suffix is never reordered or rewritten. Values beyond the
  /// live history are clamped, while zero is a no-op so a current turn cannot be
  /// summarized accidentally when no prior history exists.
  pub fn compact_prefix(
    &mut self,
    turn_id: &TurnId,
    prefix_end: usize,
    summary: &str,
  ) -> Result<u32, TurnError> {
    let prefix_end = prefix_end.min(self.messages.len());
    self.compact_range(
      turn_id,
      prefix_end,
      summary,
      ContextLevel::L1Ordinary,
      format!("summarizing {prefix_end} oldest messages"),
    )
  }

  /// Shared durable lifecycle for prefix compaction. All fallible trace writes
  /// happen before the live vector is changed, so a sink failure cannot fabricate
  /// a successful recovery or leave model-visible history half-mutated.
  fn compact_range(
    &mut self,
    turn_id: &TurnId,
    prefix_end: usize,
    summary: &str,
    level: ContextLevel,
    reason: String,
  ) -> Result<u32, TurnError> {
    let prefix_end = prefix_end.min(self.messages.len());
    self.normalize_message_seqs();
    let protected = self.checkpoint_floor.min(self.messages.len());
    if prefix_end <= protected {
      return Ok(0);
    }
    let replaced = prefix_end - protected;
    let retained = self.messages.len() - prefix_end;
    let replaced_count = u32::try_from(replaced)
      .map_err(|_| TurnError::Sink("compacted message count exceeds durable limit".into()))?;
    let retained_count = u32::try_from(retained)
      .map_err(|_| TurnError::Sink("retained message count exceeds durable limit".into()))?;
    // Canonical compaction ranges describe the messages actually replaced, not
    // the retained suffix. Durable resume restores these sequence hints beside
    // each message; in-memory callers retain the historical zero sentinel.
    let replaces_from = self.message_seqs[protected..prefix_end]
      .iter()
      .find_map(|seq| *seq)
      .unwrap_or_else(|| self.first_cited_seq());
    let replaces_through = self.message_seqs[protected..prefix_end]
      .iter()
      .rev()
      .find_map(|seq| *seq)
      .unwrap_or(replaces_from);
    let next_epoch = self.next_context_epoch()?;

    self.emit(
      Some(turn_id.clone()),
      AgentEvent::ContextCompactionStarted(ContextCompactionStarted { level, reason }),
    )?;

    // Persist the summary before deleting replaced live context. It is a user
    // message because providers accept that role mid-conversation. A checkpoint
    // capsule at the front is an impermeable floor: only messages after it may
    // be replaced by this ordinary compaction.
    let summary_message = Message::user(summary);
    let summary_envelope = self.emit_message(
      Some(turn_id.clone()),
      AgentEvent::ContextSummary,
      &summary_message,
    )?;
    let summary_ref = self.trace.put_payload(summary.as_bytes())?;
    self.emit(
      Some(turn_id.clone()),
      AgentEvent::ContextCompactionEpoch(ContextCompactionEpoch {
        context_epoch: next_epoch,
        summary: summary_ref,
        replaces_from,
        replaces_through,
      }),
    )?;
    self.emit(
      Some(turn_id.clone()),
      AgentEvent::ContextCompactionCompleted(ContextCompactionCompleted {
        level,
        removed_messages: replaced_count,
        retained_messages: retained_count,
        context_epoch: next_epoch,
      }),
    )?;

    self
      .messages
      .splice(protected..prefix_end, [summary_message]);
    self
      .message_seqs
      .splice(protected..prefix_end, [summary_envelope.meta.seq]);
    self.context_epoch = next_epoch;
    self.last_compaction = Some(Instant::now());
    Ok(replaced_count)
  }

  /// Compact oldest messages using either an explicit summary or a synthesized one.
  pub fn compact_with_summary_or(
    &mut self,
    turn_id: &TurnId,
    target_tokens: u64,
    explicit_summary: Option<&str>,
  ) -> Result<u32, TurnError> {
    let protected = self.checkpoint_floor.min(self.messages.len());
    let tail_len = self.messages.len().saturating_sub(protected);
    // A checkpoint capsule plus at most one post-checkpoint message has no
    // replaceable history. In particular, do not let the absolute prefix
    // arithmetic below produce a slice whose end precedes the capsule floor.
    if tail_len <= 1 {
      return Ok(0);
    }
    let mut kept = 1usize;
    while kept < tail_len.saturating_sub(1) {
      let next_kept = kept + 1;
      let start = self.messages.len() - next_kept;
      if estimate_messages(&self.messages[start..]) > target_tokens {
        break;
      }
      kept = next_kept;
    }
    let prefix_end = self.messages.len().saturating_sub(kept);
    if prefix_end <= protected {
      return Ok(0);
    }
    let summary_text = match explicit_summary {
      Some(text) => text.to_string(),
      None => {
        let slice = &self.messages[protected..prefix_end];
        match &self.summarizer {
          Some(custom) => custom(slice),
          None => structured_summary(slice),
        }
      }
    };
    self.compact(turn_id, &summary_text, kept)
  }

  /// Compact oldest messages using the configured summarizer.
  pub fn compact_with_summary(
    &mut self,
    turn_id: &TurnId,
    target_tokens: u64,
  ) -> Result<u32, TurnError> {
    self.compact_with_summary_or(turn_id, target_tokens, None)
  }

  /// Perform L2 semantic phase compaction across a task boundary.
  ///
  /// Restricts execution to safe boundaries, enforces a cooldown / rearm gate
  /// (bypassed when `force` is true), emits `ContextCompactionStarted` with level `L2Phase`,
  /// writes a structured phase summary message, records `ContextCompactionEpoch`,
  /// and emits `ContextCompactionCompleted` with level `L2Phase`.
  pub fn compact_phase(
    &mut self,
    turn_id: &TurnId,
    phase: &str,
    explicit_summary: Option<&str>,
    force: bool,
  ) -> Result<u32, TurnError> {
    if self.messages.len() <= 1 {
      return Ok(0);
    }

    // Cooldown gate: 5 seconds cooldown unless force is specified
    if !force {
      if let Some(last) = self.last_compaction {
        if last.elapsed() < Duration::from_secs(5) {
          self.diagnostic(
            Some(turn_id.clone()),
            DiagnosticLevel::Info,
            format!("phase compaction '{phase}' deferred: cooldown active (use force to override)"),
          )?;
          return Ok(0);
        }
      }
    }

    let kept = 1usize;
    let removed = self.messages.len() - kept;
    if removed == 0 {
      return Ok(0);
    }

    let base_summary = match explicit_summary {
      Some(text) => text.to_string(),
      None => {
        let protected = self.checkpoint_floor.min(self.messages.len());
        let slice = &self.messages[protected..removed];
        match &self.summarizer {
          Some(custom) => custom(slice),
          None => structured_summary(slice),
        }
      }
    };
    let summary_text = format!("[Phase Compaction: {phase}]\n{base_summary}");
    self.compact_range(
      turn_id,
      removed,
      &summary_text,
      ContextLevel::L2Phase,
      format!("semantic phase: {phase}"),
    )
  }

  /// Synthesize a structured ContextCapsule from messages and context state.
  pub fn synthesize_capsule(&self, state: &ContextState, reason: &str) -> ContextCapsule {
    if let Some(custom) = &self.checkpointer {
      return custom(&self.messages, state);
    }
    let current_state = format!(
      "Context pressure ({} tokens) in epoch {}; reason: {reason}",
      state.effective_tokens(),
      state.context_epoch
    );
    coding_capsule(&self.messages, self.system.as_deref(), &current_state)
  }

  /// Create a checkpoint capsule from current session state, store it, emit
  /// `CheckpointCreated`, and reset visible messages to the capsule representation.
  pub fn checkpoint(
    &mut self,
    turn_id: &TurnId,
    capsule: ContextCapsule,
  ) -> Result<CheckpointCreated, TurnError> {
    self.normalize_message_seqs();
    let protected = self.checkpoint_floor.min(self.messages.len());
    let removed = self.messages.len().saturating_sub(protected);
    let summarized_events = u64::try_from(removed)
      .map_err(|_| TurnError::Sink("checkpoint message count exceeds durable limit".into()))?;
    let next_context_epoch = self.next_context_epoch()?;
    let (checkpoint_id, path) = match self.trace.create_checkpoint(&capsule)? {
      Some((id, p)) => (id, p),
      None => {
        let id = CheckpointId::new();
        let p = format!("checkpoints/{id}.json");
        (id, p)
      }
    };
    self
      .trace
      .set_checkpoint_context_epoch(next_context_epoch)?;

    let event = CheckpointCreated {
      checkpoint_id,
      capsule_version: capsule.version,
      summarized_events,
      path,
      context_epoch: next_context_epoch,
    };

    let checkpoint_envelope = self.emit(
      Some(turn_id.clone()),
      AgentEvent::CheckpointCreated(event.clone()),
    )?;
    self.checkpoint_cited_from = match checkpoint_envelope.meta.seq {
      Some(seq) => Some(EventSeq(seq.0.checked_add(1).ok_or_else(|| {
        TurnError::Sink("checkpoint sequence space is exhausted".into())
      })?)),
      None => None,
    };

    // Reset visible messages: replace summarized history with the capsule's model representation
    let capsule_msg = Message::user(capsule.format_for_model());
    self.messages.clear();
    self.message_seqs.clear();
    self.messages.push(capsule_msg);
    self.message_seqs.push(None);
    self.checkpoint_floor = 1;

    self.context_epoch = next_context_epoch;
    self.last_compaction = Some(Instant::now());

    self.emit(
      Some(turn_id.clone()),
      AgentEvent::ContextCompactionCompleted(ContextCompactionCompleted {
        level: ContextLevel::L3Checkpoint,
        removed_messages: u32::try_from(removed)
          .map_err(|_| TurnError::Sink("checkpoint message count exceeds durable limit".into()))?,
        retained_messages: 1,
        context_epoch: self.context_epoch,
      }),
    )?;

    Ok(event)
  }

  /// Checkpoint only pre-turn history while a turn is active. The current-turn
  /// suffix remains verbatim so an automatic checkpoint cannot undermine the
  /// same recovery boundary used for provider overflow.
  fn checkpoint_turn_prefix(
    &mut self,
    turn_id: &TurnId,
    capsule: ContextCapsule,
    turn_history_start: &mut usize,
  ) -> Result<Option<CheckpointCreated>, TurnError> {
    self.normalize_message_seqs();
    let prefix_end = (*turn_history_start).min(self.messages.len());
    if prefix_end == 0 || (self.checkpoint_floor > 0 && prefix_end <= 1) {
      return Ok(None);
    }
    let protected = self.checkpoint_floor.min(prefix_end);
    let replaced = prefix_end.saturating_sub(protected);
    let summarized_events = u64::try_from(replaced)
      .map_err(|_| TurnError::Sink("checkpoint message count exceeds durable limit".into()))?;
    let (checkpoint_id, path) = match self.trace.create_checkpoint(&capsule)? {
      Some((id, path)) => (id, path),
      None => {
        let id = CheckpointId::new();
        let path = format!("checkpoints/{id}.json");
        (id, path)
      }
    };
    let next_epoch = self.next_context_epoch()?;
    self.trace.set_checkpoint_context_epoch(next_epoch)?;
    let event = CheckpointCreated {
      checkpoint_id,
      capsule_version: capsule.version,
      summarized_events,
      path,
      context_epoch: next_epoch,
    };
    let checkpoint_envelope = self.emit(
      Some(turn_id.clone()),
      AgentEvent::CheckpointCreated(event.clone()),
    )?;
    self.checkpoint_cited_from = match checkpoint_envelope.meta.seq {
      Some(seq) => Some(EventSeq(seq.0.checked_add(1).ok_or_else(|| {
        TurnError::Sink("checkpoint sequence space is exhausted".into())
      })?)),
      None => None,
    };

    let capsule_message = Message::user(capsule.format_for_model());
    let retained_tail = self.messages.len() - prefix_end;
    // The durable L3 count describes the complete post-boundary working set:
    // the protected capsule plus the untouched current-turn suffix. Full
    // checkpoints already use this same convention with a count of one.
    let retained = retained_tail
      .checked_add(1)
      .ok_or_else(|| TurnError::Sink("retained message count exceeds durable limit".into()))?;
    let removed_count = u32::try_from(replaced)
      .map_err(|_| TurnError::Sink("checkpoint message count exceeds durable limit".into()))?;
    self.emit(
      Some(turn_id.clone()),
      AgentEvent::ContextCompactionCompleted(ContextCompactionCompleted {
        level: ContextLevel::L3Checkpoint,
        removed_messages: removed_count,
        retained_messages: u32::try_from(retained)
          .map_err(|_| TurnError::Sink("retained message count exceeds durable limit".into()))?,
        context_epoch: next_epoch,
      }),
    )?;

    self.messages.splice(0..prefix_end, [capsule_message]);
    self.message_seqs.splice(0..prefix_end, [None]);
    self.checkpoint_floor = 1;
    *turn_history_start = 1;
    self.context_epoch = next_epoch;
    self.last_compaction = Some(Instant::now());
    Ok(Some(event))
  }

  fn next_context_epoch(&self) -> Result<u32, TurnError> {
    self
      .context_epoch
      .checked_add(1)
      .ok_or_else(|| TurnError::Sink("context epoch space is exhausted".into()))
  }

  /// Compact completed assistant/tool interactions from the active turn.
  ///
  /// A long coding turn is not indivisible: once a tool result is terminal, the
  /// completed interaction can be folded into a structured summary. The summary
  /// replaces the whole visible prefix after a checkpoint floor, which keeps the
  /// existing durable compaction projection valid and preserves the objective in
  /// the summary rather than deleting the original user request from resume state.
  fn compact_completed_turn_cycles(
    &mut self,
    turn_id: &TurnId,
    turn_history_start: &mut usize,
    target_tokens: u64,
    reason: String,
  ) -> Result<u32, TurnError> {
    let cycle_start = (*turn_history_start).min(self.messages.len());
    let protected = self.checkpoint_floor.min(self.messages.len());
    if cycle_start >= self.messages.len() || cycle_start < protected {
      return Ok(0);
    }

    let mut selected: Option<(usize, String)> = None;
    for boundary in (cycle_start + 1)..=self.messages.len() {
      if !safe_completed_cycle_boundary(&self.messages, cycle_start, boundary) {
        continue;
      }
      let source = self.messages[protected..boundary].to_vec();
      let summary = match &self.summarizer {
        Some(summarizer) => summarizer(&source),
        None => format_coding_summary(
          &source,
          self.system.as_deref(),
          &format!("Active turn continues after context pressure: {reason}"),
        ),
      };
      let mut candidate = self.messages[..protected].to_vec();
      candidate.push(Message::user(summary.clone()));
      candidate.extend(self.messages[boundary..].iter().cloned());
      selected = Some((boundary, summary));
      if estimate_tokens(&self.assemble_request(candidate)) <= target_tokens {
        break;
      }
    }

    let Some((prefix_end, summary)) = selected else {
      return Ok(0);
    };
    let replaced = self.compact_range(
      turn_id,
      prefix_end,
      &summary,
      ContextLevel::L1Ordinary,
      reason,
    )?;
    if replaced > 0 {
      // The summary is now the first post-checkpoint message. Subsequent cycles
      // remain part of the same active turn and can be reduced again if needed.
      *turn_history_start = self.checkpoint_floor.saturating_add(1);
    }
    Ok(replaced)
  }

  /// Compact only pre-turn history while a turn is active, preserving the
  /// current-turn suffix and updating its boundary after replacement.
  fn compact_turn_prefix(
    &mut self,
    turn_id: &TurnId,
    turn_history_start: &mut usize,
    level: ContextLevel,
    reason: String,
  ) -> Result<u32, TurnError> {
    let prefix_end = (*turn_history_start).min(self.messages.len());
    let protected = self.checkpoint_floor.min(prefix_end);
    if prefix_end <= protected {
      return Ok(0);
    }
    let prefix = self.messages[protected..prefix_end].to_vec();
    let summary = match &self.summarizer {
      Some(summarizer) => summarizer(&prefix),
      None => structured_summary(&prefix),
    };
    let removed = self.compact_range(turn_id, prefix_end, &summary, level, reason)?;
    if removed > 0 {
      *turn_history_start = self.checkpoint_floor.saturating_add(1);
    }
    Ok(removed)
  }

  /// Assemble the active provider's exact request shape without consulting or
  /// mutating context policy. Emergency overflow recovery uses this constructor.
  fn assemble_request(&self, messages: Vec<Message>) -> ModelRequest {
    self.assemble_request_for(self.provider(), messages)
  }

  /// Assemble a request as a particular attached model would receive it. The
  /// failover rebudget gate uses this before changing the active epoch.
  fn assemble_request_for(
    &self,
    provider: &dyn ModelProvider,
    messages: Vec<Message>,
  ) -> ModelRequest {
    let capabilities = provider.capabilities();
    let may_approve_mutations =
      self.interactive_tool_approval || self.tools.auto_approves_mutating();
    let tools = if self.tools_enabled && capabilities.tools {
      self
        .tools
        .specs()
        .into_iter()
        .filter(|spec| {
          self.progress_tool_is_exposed(&spec.name)
            && (may_approve_mutations
              || self
                .tools
                .metadata_for(&spec.name)
                .is_some_and(|metadata| metadata.read_only))
        })
        .collect()
    } else {
      Vec::new()
    };
    let mut request = ModelRequest::new(provider.model().clone(), capabilities, messages)
      .with_tools(tools)
      .with_thinking(self.thinking);
    let mut system = self.system.clone().unwrap_or_default();
    if !system.is_empty() {
      system.push_str("\n\n");
    }
    system.push_str(&tool_availability_prompt(&request.tools));
    request = request.with_system(system);
    request
  }

  fn estimate_request_for(&self, provider: &dyn ModelProvider, messages: Vec<Message>) -> u64 {
    estimate_tokens(&self.assemble_request_for(provider, messages))
  }

  fn estimate_request_after_eviction(
    &self,
    provider: &dyn ModelProvider,
    start: usize,
    dropped: usize,
  ) -> u64 {
    let mut messages = self.messages.clone();
    let end = start.saturating_add(dropped).min(messages.len());
    if start < end {
      messages.drain(start..end);
    }
    self.estimate_request_for(provider, messages)
  }

  /// Whether a tool remains available after the opt-in progress boundary has
  /// activated. The registry remains the authority for risk metadata; the
  /// allowlist only narrows it and never turns a read-only tool into progress.
  fn progress_tool_is_exposed(&self, name: &str) -> bool {
    if !self.progress_boundary_active {
      return true;
    }
    let Some(metadata) = self.tools.metadata_for(name) else {
      return false;
    };
    if metadata.read_only {
      return false;
    }
    self.progress_tool_names.is_empty()
      || self
        .progress_tool_names
        .iter()
        .any(|candidate| candidate == name)
  }

  /// A requested tool counts as progress only when it is both permitted and
  /// classified as mutating. This records an attempted boundary crossing; the
  /// existing tool lifecycle still decides whether its side effect is
  /// Succeeded, Failed, or Unknown.
  fn call_makes_progress(&self, call: &ToolCallBlock) -> bool {
    let Some(metadata) = self.tools.metadata_for(&call.name) else {
      return false;
    };
    !metadata.read_only
      && (self.progress_tool_names.is_empty()
        || self
          .progress_tool_names
          .iter()
          .any(|candidate| candidate == &call.name))
  }

  /// Record a no-progress request and, at the configured threshold, add a
  /// durable model-visible nudge before narrowing the following request.
  fn observe_progress(
    &mut self,
    turn_id: &TurnId,
    progress_succeeded: bool,
  ) -> Result<(), TurnError> {
    let Some(limit) = self.progress_request_limit else {
      return Ok(());
    };
    if progress_succeeded {
      self.progress_requests_without_progress = 0;
      self.progress_boundary_active = false;
      self.progress_boundary_used = true;
      return Ok(());
    }
    if self.progress_boundary_used {
      return Ok(());
    }
    self.progress_requests_without_progress =
      self.progress_requests_without_progress.saturating_add(1);
    if self.progress_requests_without_progress >= limit && !self.progress_boundary_active {
      self.progress_boundary_active = true;
      self.append_progress_instruction(turn_id)?;
      let tools = if self.progress_tool_names.is_empty() {
        "permitted mutating tools".to_string()
      } else {
        self.progress_tool_names.join(", ")
      };
      self.diagnostic(
        Some(turn_id.clone()),
        DiagnosticLevel::Info,
        format!(
          "progress boundary active after {} model request(s) without a configured progress tool; next request exposes {tools}",
          self.progress_requests_without_progress
        ),
      )?;
    }
    Ok(())
  }

  /// Put the boundary in the same model-visible history path as the existing
  /// request-budget finalization instruction. It is runtime-owned guidance, not
  /// a claim that the user wrote these words.
  fn append_progress_instruction(&mut self, turn_id: &TurnId) -> Result<(), TurnError> {
    let tools = if self.progress_tool_names.is_empty() {
      "a permitted mutating tool".to_string()
    } else {
      self.progress_tool_names.join(", ")
    };
    let text = format!(
      "Runtime progress boundary: this implementation turn has spent the configured inspection budget without calling a progress tool. In your next response, call one of {tools} to make the requested change. Do not spend another request reading, probing, or planning; the turn remains incomplete until the change is attempted."
    );
    let message = Message::user(text.clone());
    let envelope = self.emit_message(
      Some(turn_id.clone()),
      AgentEvent::UserMessage(UserMessage {
        text,
        attachments: 0,
      }),
      &message,
    )?;
    self.push_message(message, envelope.meta.seq);
    Ok(())
  }

  /// Build the model request, consulting the context policy first.
  fn build_request(
    &mut self,
    turn_id: &TurnId,
    turn_history_start: &mut usize,
  ) -> Result<ModelRequest, TurnError> {
    let capabilities = self.provider().capabilities();
    let estimated_request = self.assemble_request(self.messages.clone());
    let estimated_tokens = estimate_tokens(&estimated_request);
    let state = {
      let mut state = ContextState::zero(capabilities.context_window);
      state.model = Some(self.active_model());
      state.context_epoch = self.context_epoch;
      // Provider usage describes the preceding request, not this assembled one.
      // Use the current estimate until an exact measurement for this request is
      // available; never substitute a stale value from another prompt or model.
      state.estimated_tokens = estimated_tokens;
      state.recent_tokens =
        estimate_messages(&self.messages[(*turn_history_start).min(self.messages.len())..]);
      state.working_messages = self.messages.len() as u32;
      // A loop that has never compacted has waited longer than any cooldown:
      // u64::MAX states that without inventing a timestamp.
      state.since_last_compaction_ms = self.last_compaction.map_or(u64::MAX, elapsed_ms);
      // Nothing is in flight and no call is pending at this point, which is what
      // makes it the safe boundary.
      state.at_safe_boundary = true;
      state
    };
    let decision = self.context.evaluate(&state);
    match decision.action {
      // Refusal is the honest answer to a request that cannot fit. Truncating
      // history here would silently change what the model was asked.
      ContextAction::Refuse { reason } => {
        self.diagnostic(
          None,
          DiagnosticLevel::Error,
          format!("context refused: {reason}"),
        )?;
        return Err(TurnError::Aborted(TurnStatus::Failed {
          kind: ModelFailureKind::ContextOverflow,
        }));
      }
      // When compaction is recommended, summarizing compaction opens a durable
      // epoch if configured. Otherwise, pre-emptive eviction sheds oldest turns.
      ContextAction::Compact {
        level,
        reason,
        target_tokens,
      } => {
        let mut compacted = 0;
        if *turn_history_start > 0
          && (level == ContextLevel::L2Phase
            || self.compaction_strategy == CompactionStrategy::Summarize
            || self.summarizer.is_some())
        {
          compacted =
            self.compact_turn_prefix(turn_id, turn_history_start, level, reason.clone())?;
        }
        if compacted == 0 {
          compacted = self.compact_completed_turn_cycles(
            turn_id,
            turn_history_start,
            target_tokens,
            reason.clone(),
          )?;
        }
        if compacted == 0 && self.evict_oldest(target_tokens, turn_id, turn_history_start)? == 0 {
          // Nothing could be dropped: the newest turn alone is over the target.
          // The recommendation is the surface's again, so it stays visible.
          self.diagnostic(
            None,
            DiagnosticLevel::Warn,
            format!("context suggests {} compaction: {reason}", level.as_str()),
          )?;
        }
      }
      ContextAction::SuggestCheckpoint { reason } => {
        if self.checkpoint_strategy == CheckpointStrategy::Auto {
          let capsule = self.synthesize_capsule(&state, &reason);
          let checkpoint = if *turn_history_start > 0 {
            self.checkpoint_turn_prefix(turn_id, capsule, turn_history_start)?
          } else {
            // The only resident messages are from this turn. Resetting them
            // would erase content that overflow recovery is required to keep.
            None
          };
          match checkpoint {
            Some(created) => {
              self.diagnostic(
                Some(turn_id.clone()),
                DiagnosticLevel::Info,
                format!(
                  "runtime created checkpoint {} under context pressure: {reason}",
                  created.checkpoint_id
                ),
              )?;
            }
            None => {
              self.diagnostic(
                Some(turn_id.clone()),
                DiagnosticLevel::Warn,
                format!("context suggests checkpoint: {reason}"),
              )?;
            }
          }
        } else {
          self.diagnostic(
            Some(turn_id.clone()),
            DiagnosticLevel::Warn,
            format!("context suggests checkpoint: {reason}"),
          )?;
        }
      }
      ContextAction::ReducePayload { reason } => {
        let target_tokens = match reason {
          ReductionReason::RecentTargetExceeded { target_tokens } => target_tokens,
          _ => estimate_messages(&self.messages).saturating_mul(3) / 4,
        };
        let compacted = self.compact_completed_turn_cycles(
          turn_id,
          turn_history_start,
          target_tokens,
          format!("payload reduction: {reason:?}"),
        )?;
        if compacted == 0 {
          // Older complete turns still benefit from the established L0 eviction
          // path when the active turn has no completed interaction boundary yet.
          let evicted = self.evict_oldest(target_tokens, turn_id, turn_history_start)?;
          if evicted == 0 {
            self.diagnostic(
              Some(turn_id.clone()),
              DiagnosticLevel::Info,
              "context requested payload reduction, but no completed history boundary is available yet",
            )?;
          }
        }
      }
      ContextAction::Warn { .. } | ContextAction::Keep => {}
    }

    Ok(self.assemble_request(self.messages.clone()))
  }

  /// Persist one assistant response and add its visible content to model history.
  ///
  /// Keeping this transaction in one helper is important for automatic budget
  /// finalization: its no-tool response must receive exactly the same durable
  /// assistant-message treatment as an ordinary model round.
  fn record_response(
    &mut self,
    response: Response,
    report: &mut TurnReport,
  ) -> Result<RecordedResponse, TurnError> {
    let Response {
      epoch,
      text,
      calls,
      rejected_calls,
      mut completion,
    } = response;
    report.epoch = epoch;
    report.requests = self.requests.load(Ordering::SeqCst);

    let mut blocks = Vec::new();
    if let Some(text) = text.filter(|text| !text.is_empty()) {
      blocks.push(ContentBlock::text(text.clone()));
      report.text.push_str(&text);
    }
    for call in &calls {
      blocks.push(ContentBlock::ToolCall(ToolCallBlock {
        id: call.id.clone(),
        name: call.name.clone(),
        arguments: call.arguments.clone(),
      }));
    }
    let assistant_event_id = completion.meta.event_id.clone();
    if !blocks.is_empty() {
      let message = Message::new(Role::Assistant, blocks);
      self.trace.emit_message(&mut completion, &message)?;
      self.envelopes.push(completion.clone());
      self.push_message(message, completion.meta.seq);
    } else {
      self.trace.emit_without_message(&mut completion)?;
      self.envelopes.push(completion);
    }
    Ok(RecordedResponse {
      assistant_event_id,
      calls,
      rejected_calls,
    })
  }

  /// Add the runtime-owned instruction that explains why the final request has no tools.
  fn append_finalization_instruction(&mut self, turn_id: &TurnId) -> Result<(), TurnError> {
    let text = "The model-request safety budget is exhausted for this turn. This is a bounded finalization request: do not request or imply any tool execution. Summarize what is complete, identify unfinished files or verification, and state the safest next continuation step. Treat the task as incomplete.";
    let message = Message::user(text);
    let envelope = self.emit_message(
      Some(turn_id.clone()),
      AgentEvent::UserMessage(UserMessage {
        text: text.to_string(),
        attachments: 0,
      }),
      &message,
    )?;
    self.push_message(message, envelope.meta.seq);
    Ok(())
  }

  /// Close fully decoded calls from a response that cannot be acted on.
  fn record_unexecuted_calls(
    &mut self,
    turn_id: TurnId,
    assistant_event_id: rupi_core::EventId,
    calls: &[ToolCallBlock],
    progress: &mut dyn TurnProgress,
    reason: &str,
  ) -> Result<(), TurnError> {
    for call in calls {
      let read_only = self
        .tools
        .metadata_for(&call.name)
        .map(|metadata| metadata.read_only)
        .unwrap_or(false);
      progress.on_tool_requested(call);
      let requested = self.emit_with_parent(
        Some(turn_id.clone()),
        AgentEvent::ToolRequested(ToolRequested {
          call_id: call.id.clone(),
          name: call.name.clone(),
          arguments: call.arguments.clone(),
          read_only,
        }),
        Some(assistant_event_id.clone()),
      )?;
      let outcome = ToolOutcome::failed(reason.to_string());
      let message = Message::new(
        Role::Tool,
        vec![ContentBlock::ToolResult(
          outcome.to_block(call.id.clone(), &call.name),
        )],
      );
      let envelope = self.emit_message_with_parent(
        Some(turn_id.clone()),
        AgentEvent::ToolFailed(ToolFailed {
          call_id: call.id.clone(),
          name: call.name.clone(),
          message: reason.to_string(),
          duration_ms: 0,
          status: None,
        }),
        &message,
        Some(requested.meta.event_id),
      )?;
      self.push_message(message, envelope.meta.seq);
      let executed = Executed {
        request: rupi_core::ToolRequest {
          call_id: call.id.clone(),
          name: call.name.clone(),
          arguments: call.arguments.clone(),
        },
        outcome,
        state: ToolExecutionState::Failed,
        started: false,
        refusal: Some(reason.to_string()),
        full_output: None,
        cancelled: false,
      };
      progress.on_tool_finished(call, &executed);
    }
    Ok(())
  }

  /// Execute the calls one completed request asked for, in declaration order.
  fn execute_calls(
    &mut self,
    turn_id: TurnId,
    assistant_event_id: rupi_core::EventId,
    calls: &[ToolCallBlock],
    rejected_calls: &BTreeMap<String, String>,
    cancel: &CancelToken,
    progress: &mut dyn TurnProgress,
  ) -> Result<bool, TurnError> {
    let mut progress_succeeded = false;
    for (index, call) in calls.iter().enumerate() {
      if cancel.is_cancelled() {
        // The assistant batch is already committed. Close the entire remaining
        // tail before allowing another provider request or session continuation.
        self.record_unexecuted_calls(
          turn_id.clone(),
          assistant_event_id.clone(),
          &calls[index..],
          progress,
          "not executed: the turn was cancelled",
        )?;
        break;
      }
      let metadata = self.tools.metadata_for(&call.name);
      let read_only = metadata
        .as_ref()
        .map(|meta| meta.read_only)
        .unwrap_or(false);
      progress.on_tool_requested(call);
      let requested = self.emit_with_parent(
        Some(turn_id.clone()),
        AgentEvent::ToolRequested(ToolRequested {
          call_id: call.id.clone(),
          name: call.name.clone(),
          arguments: call.arguments.clone(),
          read_only,
        }),
        Some(assistant_event_id.clone()),
      )?;

      if let Some(parse_reason) = rejected_calls.get(call.id.as_str()) {
        let reason = format!(
          "The tool call was not run because its model-generated request was invalid: {parse_reason}. Send a corrected tool call with complete JSON object arguments."
        );
        let executed = Executed {
          request: rupi_core::ToolRequest {
            call_id: call.id.clone(),
            name: call.name.clone(),
            arguments: call.arguments.clone(),
          },
          outcome: ToolOutcome::failed(reason.clone()),
          state: ToolExecutionState::Failed,
          started: false,
          refusal: Some(reason),
          full_output: None,
          cancelled: false,
        };
        let (block, seq) = self.record_tool_outcome(
          turn_id.clone(),
          call,
          &executed,
          0,
          read_only,
          Some(requested.meta.event_id),
        )?;
        progress.on_tool_finished(call, &executed);
        self.push_message(
          Message::new(Role::Tool, vec![ContentBlock::ToolResult(block)]),
          seq,
        );
        continue;
      }

      let request = rupi_core::ToolRequest {
        call_id: call.id.clone(),
        name: call.name.clone(),
        arguments: call.arguments.clone(),
      };
      let attribution = StreamAttribution {
        turn_id: turn_id.clone(),
        session_id: self.session_id.clone(),
        trace_id: self.trace_id.clone(),
        epoch: self.epoch_index(),
        model: self.active_model(),
      };
      let approval = match metadata.as_ref().filter(|metadata| !metadata.read_only) {
        None => Approval::Allow,
        Some(_) if self.tools.auto_approves_mutating() => Approval::Allow,
        Some(metadata) if self.interactive_tool_approval => {
          progress.approve_mutating_tool(metadata, &call.arguments)
        }
        Some(_) => Approval::Deny(
          "approval is required for this mutating tool, but this surface cannot ask; nothing was changed"
            .into(),
        ),
      };
      let executed = {
        let mut gate = FixedApprovalGate(approval);
        let mut sink = LiveToolSink { progress, call };
        let trace = &mut *self.trace;
        let mut started_event_id = None;
        let mut on_started = || {
          let mut meta =
            EventMeta::new(attribution.session_id.clone(), attribution.trace_id.clone());
          meta.turn_id = Some(attribution.turn_id.clone());
          meta.model_epoch = Some(attribution.epoch);
          meta.model = Some(attribution.model.clone());
          meta.parent_event_id = Some(requested.meta.event_id.clone());
          let mut envelope = EventEnvelope::new(
            meta,
            AgentEvent::ToolStarted(ToolStarted {
              call_id: call.id.clone(),
              name: call.name.clone(),
            }),
          );
          let result = trace.emit(&mut envelope);
          if result.is_ok() {
            started_event_id = Some(envelope.meta.event_id.clone());
          }
          result
        };
        let clock = Instant::now();
        let executed = self.tools.execute_observed_with_gate(
          &request,
          &mut sink,
          cancel,
          &mut gate,
          &mut on_started,
        )?;
        (executed, elapsed_ms(clock), started_event_id)
      };
      let (mut execution, duration_ms, started_event_id) = executed;
      // The registry can observe cancellation in the small race between our batch
      // check and dispatch. It proves this call never started, so close it as a
      // terminal failure rather than leaving an open Requested lifecycle.
      if execution.state == ToolExecutionState::Requested && !execution.started {
        execution.state = ToolExecutionState::Failed;
        execution.outcome.state = ToolExecutionState::Failed;
        execution.outcome.is_error = true;
      }
      let (block, seq) = self.record_tool_outcome(
        turn_id.clone(),
        call,
        &execution,
        duration_ms,
        read_only,
        started_event_id.or(Some(requested.meta.event_id.clone())),
      )?;
      if self.call_makes_progress(call) && execution.state == ToolExecutionState::Succeeded {
        progress_succeeded = true;
      }
      progress.on_tool_finished(call, &execution);
      self.push_message(
        Message::new(Role::Tool, vec![ContentBlock::ToolResult(block)]),
        seq,
      );
    }
    Ok(progress_succeeded)
  }

  /// Turn one executed call into its session block and its terminal event.
  fn record_tool_outcome(
    &mut self,
    turn_id: TurnId,
    call: &ToolCallBlock,
    executed: &Executed,
    duration_ms: u64,
    read_only: bool,
    parent_event_id: Option<rupi_core::EventId>,
  ) -> Result<(ToolResultBlock, Option<EventSeq>), TurnError> {
    let outcome = &executed.outcome;
    let text = outcome.text.clone();
    let mut reduced = outcome.reduced;
    let mut recovery_blob = None;

    if let Some(full) = executed.full_output.as_ref() {
      // Reduction already happened in the registry. Here the full bytes become
      // recoverable, and the event records that the model saw a summary.
      let blob = self.trace.put_payload(full)?;
      reduced = true;
      recovery_blob = blob.clone();
      let recovery_ref = blob.as_ref().map(BlobRef::recovery_ref);
      self.emit(
        Some(turn_id.clone()),
        AgentEvent::ContextReduced(ContextReduced {
          reason: ReductionReason::OversizedToolOutput {
            limit_bytes: full.len() as u64,
          },
          original_bytes: full.len() as u64,
          visible_bytes: text.len() as u64,
          removed_messages: 0,
          retained_messages: 0,
          recovery_ref,
          blob,
          tool_call_id: Some(call.id.clone()),
        }),
      )?;
    }

    // Keep the risk classification captured alongside the request. A dynamic
    // registry may replace or unregister the tool while it runs; consulting
    // the current map here could claim that an uncertain side effect was
    // read-only (or vice versa).
    let mutating = !read_only;

    let event = match executed.state {
      ToolExecutionState::Succeeded => AgentEvent::ToolCompleted(ToolCompleted {
        call_id: call.id.clone(),
        name: call.name.clone(),
        state: ToolExecutionState::Succeeded,
        duration_ms,
        status: outcome.status,
        reduced,
        blob: recovery_blob,
        visible_bytes: text.len() as u64,
      }),
      ToolExecutionState::Failed => AgentEvent::ToolFailed(ToolFailed {
        call_id: call.id.clone(),
        name: call.name.clone(),
        message: text.clone(),
        duration_ms,
        status: outcome.status,
      }),
      // Uncertain, refused, and never-started calls all need a terminal event, and
      // `tool_unknown` is the only honest one for any of them: the runtime does not
      // know, and must not pick between success and failure.
      ToolExecutionState::Unknown | ToolExecutionState::Requested | ToolExecutionState::Started => {
        AgentEvent::ToolUnknown(ToolUnknown {
          call_id: call.id.clone(),
          name: call.name.clone(),
          why: executed.refusal.clone().unwrap_or_else(|| text.clone()),
          mutating,
        })
      }
    };
    let block = ToolResultBlock {
      id: call.id.clone(),
      name: call.name.clone(),
      state: executed.state,
      text,
      is_error: outcome.is_error,
      reduced,
    };
    let envelope = self.emit_message_with_parent(
      Some(turn_id.clone()),
      event,
      &Message::new(Role::Tool, vec![ContentBlock::ToolResult(block.clone())]),
      parent_event_id,
    )?;

    Ok((block, envelope.meta.seq))
  }
}

/// Internal failure routing for one request attempt.
enum TurnFailure {
  /// The user stopped it. Not a fault, so never recovered from.
  Cancelled,
  /// The provider rejected an otherwise uncommitted request for context size.
  /// The outer turn loop may compact only pre-turn history and reissue once.
  ProviderOverflow(ModelFailure),
  /// No model can serve the request.
  Fatal(ModelFailure),
  /// The trace could not be written.
  Sink(SinkError),
}

impl From<SinkError> for TurnFailure {
  fn from(error: SinkError) -> Self {
    Self::Sink(error)
  }
}

impl From<TurnError> for TurnFailure {
  fn from(error: TurnError) -> Self {
    match error {
      TurnError::Sink(message) => Self::Sink(SinkError(message)),
      TurnError::Unavailable(failure) => Self::Fatal(failure),
      // An aborted turn is not a provider failure, so it is reported as one whose
      // phase says the runtime itself refused to send.
      TurnError::Aborted(_) => Self::Fatal(ModelFailure::new(
        ModelFailureKind::ContextOverflow,
        FailurePhase::PreRequest,
        "context refused the request",
      )),
      TurnError::Refused(message) => Self::Fatal(ModelFailure::new(
        ModelFailureKind::Protocol,
        FailurePhase::PreRequest,
        message,
      )),
    }
  }
}

/// Attribution used while normalized provider deltas are received.
struct StreamAttribution {
  turn_id: TurnId,
  session_id: SessionId,
  trace_id: TraceId,
  epoch: u32,
  model: ModelRef,
}

/// Tees provider output to the durable trace at receipt time and to the surface
/// only after the trace accepted it. The provider sink is infallible, so the
/// first sink error is retained and cancellation asks the transport to stop.
struct Collector<'a> {
  progress: &'a mut dyn TurnProgress,
  trace: &'a mut dyn Trace,
  attribution: StreamAttribution,
  cancel: CancelToken,
  clock: Instant,
  first_delta_ms: Option<u64>,
  text: String,
  calls: Vec<ToolCallBlock>,
  rejected_calls: BTreeMap<String, String>,
  committed: bool,
  reasoning_index: u32,
  text_index: u32,
  reasoning_provenance: Option<ReasoningProvenance>,
  sink_error: Option<SinkError>,
}

impl<'a> Collector<'a> {
  fn new(
    progress: &'a mut dyn TurnProgress,
    trace: &'a mut dyn Trace,
    attribution: StreamAttribution,
    cancel: CancelToken,
    clock: Instant,
  ) -> Self {
    Self {
      progress,
      trace,
      attribution,
      cancel,
      clock,
      first_delta_ms: None,
      text: String::new(),
      calls: Vec::new(),
      rejected_calls: BTreeMap::new(),
      committed: false,
      reasoning_index: 0,
      text_index: 0,
      reasoning_provenance: None,
      sink_error: None,
    }
  }

  fn mark_first_delta(&mut self) {
    if self.first_delta_ms.is_none() {
      self.first_delta_ms = Some(elapsed_ms(self.clock));
    }
  }

  fn trace_event(&mut self, event: AgentEvent) -> Option<EventEnvelope> {
    if self.sink_error.is_some() {
      return None;
    }
    let mut meta = EventMeta::new(
      self.attribution.session_id.clone(),
      self.attribution.trace_id.clone(),
    );
    meta.turn_id = Some(self.attribution.turn_id.clone());
    meta.model_epoch = Some(self.attribution.epoch);
    meta.model = Some(self.attribution.model.clone());
    let mut envelope = EventEnvelope::new(meta, event);
    if let Err(error) = self.trace.emit(&mut envelope) {
      self.sink_error = Some(error);
      self.cancel.cancel();
      return None;
    }
    Some(envelope)
  }
}

impl rupi_core::ProviderEventSink for Collector<'_> {
  fn emit(&mut self, event: &rupi_core::ProviderEvent) {
    self.mark_first_delta();
    match event {
      rupi_core::ProviderEvent::ReasoningDelta { text, provenance } => {
        let traced = self.trace_event(AgentEvent::ReasoningDelta(ReasoningDelta {
          text: text.clone(),
          provenance: *provenance,
          chunk_index: self.reasoning_index,
        }));
        if traced.is_some() {
          self.reasoning_index = self.reasoning_index.saturating_add(1);
          self.reasoning_provenance = Some(*provenance);
          self.committed = true;
          self.progress.on_reasoning(text, *provenance);
        }
      }
      rupi_core::ProviderEvent::TextDelta(text) => {
        let traced = self.trace_event(AgentEvent::AssistantDelta(AssistantDelta {
          text: text.clone(),
          chunk_index: self.text_index,
        }));
        if traced.is_some() {
          self.text_index = self.text_index.saturating_add(1);
          self.committed = true;
          self.text.push_str(text);
          self.progress.on_text_delta(text);
        }
      }
      rupi_core::ProviderEvent::ToolCall(call) => {
        if self.sink_error.is_none() {
          self.committed = true;
          self.calls.push(call.clone());
        }
      }
      rupi_core::ProviderEvent::ToolCallRejected { id, name, reason } => {
        if self.sink_error.is_none() {
          self.committed = true;
          self.calls.push(ToolCallBlock {
            id: id.clone(),
            name: name.clone(),
            arguments: serde_json::json!({}),
          });
          self
            .rejected_calls
            .insert(id.as_str().to_string(), reason.clone());
        }
      }
    }
  }
}

/// A failure when the response was not a completion, `None` when it was.
fn completion_failure(
  usage: &rupi_core::CompletionUsage,
  produced: bool,
  committed: bool,
) -> Option<ModelFailure> {
  if usage.stopped_at_output_limit() {
    let mut failure = ModelFailure::new(
      ModelFailureKind::Semantic,
      FailurePhase::Normalizing,
      "provider stopped at its output limit before completing the response",
    );
    failure.partial_output_emitted = committed;
    failure.detail = usage.finish_reason.clone();
    return Some(failure);
  }
  if usage.is_certain() {
    return None;
  }
  let mut failure = ModelFailure::new(
    if produced {
      ModelFailureKind::Semantic
    } else {
      ModelFailureKind::Protocol
    },
    FailurePhase::Streaming,
    "the response stream ended without a definitive completion signal",
  );
  failure.partial_output_emitted = committed;
  Some(failure)
}

/// Tool chunks are transient surface output in this slice. The canonical trace
/// records only the final reduced result under its tool lifecycle event.
struct LiveToolSink<'a> {
  progress: &'a mut dyn TurnProgress,
  call: &'a ToolCallBlock,
}

impl ToolProgress for LiveToolSink<'_> {
  fn emit(&mut self, chunk: &rupi_core::ToolChunk) {
    self.progress.on_tool_progress(self.call, &chunk.text);
  }
}

fn elapsed_ms(clock: Instant) -> u64 {
  clock.elapsed().as_millis() as u64
}

/// Rough token estimate for the request about to be sent.
///
/// The estimate deliberately includes the complete tool schema because a request
/// can fit by message bytes alone while still exceeding the provider window once
/// exposed tools are serialized. A previous request's usage is not a measurement
/// of this request and must not replace this estimate.
fn estimate_tokens(request: &ModelRequest) -> u64 {
  let mut bytes = request
    .system
    .as_ref()
    .map(|system| system.len())
    .unwrap_or(0);
  for message in &request.messages {
    bytes += estimate_message_bytes(message);
  }
  for spec in &request.tools {
    bytes += spec.name.len() + spec.description.len() + spec.parameters.to_string().len();
  }
  (bytes / 4).max(1) as u64
}

struct FixedApprovalGate(Approval);

impl ApprovalGate for FixedApprovalGate {
  fn decide(
    &mut self,
    _metadata: &rupi_core::ToolMetadata,
    _arguments: &serde_json::Value,
  ) -> Approval {
    self.0.clone()
  }
}

fn tool_availability_prompt(tools: &[rupi_core::ToolSpec]) -> String {
  if tools.is_empty() {
    return "No tools are available for this request. Do not claim to inspect or change workspace state.".into();
  }
  let names = tools
    .iter()
    .map(|tool| tool.name.as_str())
    .collect::<Vec<_>>()
    .join(", ");
  format!(
    "Tools available for this request: {names}. Use only these tools; the listed schemas define the permitted arguments."
  )
}

/// Leave ten percent of the provider's advertised window as estimation headroom.
/// The assembled request, including system text and exposed tools, is measured
/// before this target is accepted.
fn overflow_recovery_target(window: u64) -> u64 {
  window.saturating_mul(9) / 10
}

/// Truncate text without ever splitting a UTF-8 code point.
pub fn truncate_utf8_to_bytes(text: &str, max_bytes: usize) -> &str {
  let mut end = max_bytes.min(text.len());
  while end > 0 && !text.is_char_boundary(end) {
    end -= 1;
  }
  &text[..end]
}

/// Rough token estimate for history alone.
fn estimate_messages(messages: &[Message]) -> u64 {
  let bytes: usize = messages.iter().map(estimate_message_bytes).sum();
  (bytes / 4).max(1) as u64
}

fn safe_eviction_boundary(messages: &[Message], start: usize, boundary: usize, end: usize) -> bool {
  if boundary < start || boundary > end || boundary > messages.len() {
    return false;
  }
  if boundary == start {
    return true;
  }
  // A retained suffix must begin at a new user turn. This keeps assistant tool
  // calls paired with their tool results and avoids retaining a result whose
  // call was evicted with the preceding turn.
  if boundary < messages.len() && messages[boundary].role != Role::User {
    return false;
  }
  let previous = &messages[boundary - 1];
  !previous
    .content
    .iter()
    .any(|block| matches!(block, ContentBlock::ToolCall(_)))
}

/// Whether a boundary follows a complete, terminal assistant/tool interaction.
///
/// Unlike ordinary turn eviction, the retained suffix may begin with an assistant
/// tool call. The preserved summary user message immediately before it supplies the
/// protocol anchor; walking backwards proves every call in the candidate prefix has
/// a matching terminal result before allowing the cut. Unknown results are kept in
/// the visible window so a later request cannot mistake an uncertain mutation for a
/// completed cycle.
fn safe_completed_cycle_boundary(messages: &[Message], start: usize, boundary: usize) -> bool {
  if boundary <= start || boundary > messages.len() {
    return false;
  }
  if messages[boundary - 1].role != Role::Tool {
    return false;
  }

  let mut results = BTreeSet::new();
  let mut saw_tool = false;
  for message in messages[start..boundary].iter().rev() {
    match message.role {
      Role::Tool => {
        saw_tool = true;
        for block in &message.content {
          let ContentBlock::ToolResult(result) = block else {
            continue;
          };
          if result.state == ToolExecutionState::Unknown || !result.state.is_terminal() {
            return false;
          }
          if !results.insert(result.id.clone()) {
            return false;
          }
        }
      }
      Role::Assistant => {
        let calls: Vec<_> = message.tool_calls().collect();
        if calls.is_empty() {
          continue;
        }
        if !saw_tool {
          return false;
        }
        if calls.iter().any(|call| !results.remove(&call.id)) {
          return false;
        }
      }
      Role::System | Role::User => {}
    }
  }
  results.is_empty()
}

fn estimate_message_bytes(message: &Message) -> usize {
  message
    .content
    .iter()
    .map(|block| match block {
      ContentBlock::Text { text } => text.len(),
      ContentBlock::Reasoning(chunk) => chunk.text.len(),
      ContentBlock::ToolCall(call) => call.name.len() + call.arguments.to_string().len(),
      ContentBlock::ToolResult(result) => result.text.len(),
      ContentBlock::Image { data_base64, .. } => data_base64.len(),
    })
    .sum()
}

/// Synthesize a factual coding-work capsule from the visible conversation.
///
/// Tool outcomes are attributed only when their matching result is present. This
/// keeps a checkpoint from turning an unanswered or uncertain operation into a
/// claim that work completed.
fn coding_capsule(
  messages: &[Message],
  system: Option<&str>,
  current_state: &str,
) -> ContextCapsule {
  let mut objective = None;
  let mut completed_work = Vec::new();
  let mut decisions = Vec::new();
  let mut constraints = Vec::new();
  let mut artifacts = Vec::new();
  let mut unresolved = Vec::new();
  let mut next_actions = Vec::new();
  let mut prior_state = String::new();
  let mut pending_calls = BTreeMap::new();

  for message in messages {
    match message.role {
      Role::User => {
        let text = message.text();
        let trimmed = text.trim();
        if trimmed.starts_with("[Session Checkpoint Capsule]") {
          let mut section = "";
          for line in trimmed.lines() {
            let line = line.trim();
            if let Some(value) = line.strip_prefix("objective: ") {
              objective.get_or_insert_with(|| bounded_text(value, 400));
            } else if let Some(value) = line.strip_prefix("current_state: ") {
              prior_state = bounded_text(value, 400);
            } else if matches!(
              line,
              "completed:"
                | "decisions:"
                | "constraints:"
                | "important_artifacts:"
                | "unresolved:"
                | "next_actions:"
            ) {
              section = line.trim_end_matches(':');
            } else if let Some(value) = line.strip_prefix("- ") {
              match section {
                "completed" => push_unique(&mut completed_work, bounded_text(value, 240)),
                "decisions" => {
                  let (decision, rationale) = value
                    .rsplit_once(": ")
                    .unwrap_or((value, "carried forward from an earlier visible capsule"));
                  push_unique(
                    &mut decisions,
                    CapsuleDecision {
                      decision: bounded_text(decision, 240),
                      rationale: bounded_text(rationale, 240),
                    },
                  );
                }
                "constraints" => push_unique(&mut constraints, bounded_text(value, 240)),
                "important_artifacts" => {
                  if let Some((path, note)) = value.rsplit_once(": ") {
                    upsert_artifact(&mut artifacts, path, bounded_text(note, 200));
                  }
                }
                "unresolved" => push_unique(&mut unresolved, bounded_text(value, 240)),
                "next_actions" => push_unique(&mut next_actions, bounded_text(value, 240)),
                _ => {}
              }
            } else {
              section = "";
            }
          }
          continue;
        }
        if !trimmed.is_empty() && objective.is_none() {
          objective = Some(bounded_text(trimmed.lines().next().unwrap_or(trimmed), 400));
        }
        for line in trimmed
          .lines()
          .map(str::trim)
          .filter(|line| !line.is_empty())
        {
          if contains_instruction_marker(line) {
            push_unique(
              &mut constraints,
              format!("User instruction: {}", bounded_text(line, 240)),
            );
          }
        }
      }
      Role::Assistant => {
        for block in &message.content {
          match block {
            ContentBlock::Text { text } => {
              for line in text.lines().map(str::trim) {
                if let Some(decision) = line.strip_prefix("Decision:") {
                  push_unique(
                    &mut decisions,
                    CapsuleDecision {
                      decision: bounded_text(decision.trim(), 240),
                      rationale: "stated in visible assistant text".into(),
                    },
                  );
                }
              }
            }
            ContentBlock::ToolCall(call) => {
              pending_calls.insert(call.id.to_string(), call.clone());
            }
            _ => {}
          }
        }
      }
      Role::Tool => {
        for block in &message.content {
          let ContentBlock::ToolResult(result) = block else {
            continue;
          };
          let Some(call) = pending_calls.remove(&result.id.to_string()) else {
            push_unique(
              &mut unresolved,
              format!(
                "Tool result `{}` has no visible matching call.",
                result.name
              ),
            );
            continue;
          };
          let path = ["path", "file", "file_path"]
            .iter()
            .find_map(|key| call.arguments.get(key).and_then(serde_json::Value::as_str));
          let location = path
            .map(|path| format!(" for `{path}`"))
            .unwrap_or_default();
          let command = coding_command(&call);
          let is_verification = command.as_deref().is_some_and(is_verification_command);
          let outcome = match result.state {
            ToolExecutionState::Succeeded => {
              if is_verification {
                let output = bounded_text(result.text.trim(), 160);
                format!(
                  "Verification passed: `{}`{}",
                  command.as_deref().unwrap_or_default(),
                  if output.is_empty() {
                    String::new()
                  } else {
                    format!(" — {output}")
                  }
                )
              } else {
                let location = path
                  .map(|path| format!(" for `{path}`"))
                  .unwrap_or_default();
                let output = bounded_text(result.text.trim(), 100);
                format!(
                  "Tool `{}` completed{location}{}",
                  call.name,
                  if output.is_empty() {
                    ".".into()
                  } else {
                    format!("; observed: {output}")
                  }
                )
              }
            }
            ToolExecutionState::Failed => {
              let detail = bounded_text(result.text.trim(), 240);
              let status = if is_verification {
                "Verification failed"
              } else {
                "Tool failed"
              };
              format!(
                "{status}: `{}`{location}{}",
                command.as_deref().unwrap_or(&call.name),
                if detail.is_empty() {
                  String::new()
                } else {
                  format!(" — {detail}")
                }
              )
            }
            ToolExecutionState::Unknown
            | ToolExecutionState::Started
            | ToolExecutionState::Requested => {
              let detail = bounded_text(result.text.trim(), 160);
              format!(
                "Tool `{}`{location} ended in state `{}`; inspect its effects before retrying{}.",
                call.name,
                result.state.as_str(),
                if detail.is_empty() {
                  String::new()
                } else {
                  format!("; visible result: {detail}")
                }
              )
            }
          };
          match result.state {
            ToolExecutionState::Succeeded => {
              push_unique(&mut completed_work, outcome);
              if let Some(path) = path {
                let note = if matches!(call.name.as_str(), "write" | "edit" | "append") {
                  format!("modified by `{}`", call.name)
                } else {
                  format!("referenced by `{}`", call.name)
                };
                upsert_artifact(&mut artifacts, path, note);
              }
            }
            _ => push_unique(&mut unresolved, outcome),
          }
        }
      }
      Role::System => {}
    }
  }

  for call in pending_calls.values() {
    let location = ["path", "file", "file_path"]
      .iter()
      .find_map(|key| call.arguments.get(key).and_then(serde_json::Value::as_str))
      .map(|path| format!(" for `{path}`"))
      .unwrap_or_default();
    push_unique(
      &mut unresolved,
      format!(
        "Tool call `{}`{location} has no visible terminal result.",
        call.name
      ),
    );
  }
  if let Some(system) = system.filter(|system| !system.trim().is_empty()) {
    constraints.push("Follow the active system instructions.".into());
    for line in system
      .lines()
      .map(str::trim)
      .filter(|line| contains_instruction_marker(line))
    {
      push_unique(&mut constraints, bounded_text(line, 240));
    }
  }

  let mut capsule =
    ContextCapsule::new(objective.unwrap_or_else(|| "Perform assigned task".into()));
  let omitted_work = retain_recent(&mut completed_work, 32);
  let omitted_decisions = retain_recent(&mut decisions, 32);
  let omitted_artifacts = retain_recent(&mut artifacts, 32);
  capsule.completed_work = completed_work;
  capsule.decisions = decisions;
  capsule.constraints = constraints;
  capsule.current_state = match (prior_state.is_empty(), current_state.is_empty()) {
    (true, true) => String::new(),
    (true, false) => current_state.to_string(),
    (false, true) => prior_state,
    (false, false) => format!("Earlier state: {prior_state}; current state: {current_state}"),
  };
  if omitted_work || omitted_decisions || omitted_artifacts {
    if !capsule.current_state.is_empty() {
      capsule.current_state.push_str("; ");
    }
    capsule.current_state.push_str(
      "some older progress details are omitted; consult the canonical journal before claiming completeness",
    );
  }
  capsule.artifacts = artifacts;
  capsule.unresolved = unresolved;
  if capsule.unresolved.is_empty() {
    push_unique(
      &mut next_actions,
      "Continue the objective using the retained recent context.".into(),
    );
  } else {
    push_unique(
      &mut next_actions,
      "Resolve the listed failures or uncertain operations before claiming completion.".into(),
    );
  }
  retain_recent(&mut next_actions, 8);
  capsule.next_actions = next_actions;
  capsule
}

fn bounded_text(value: &str, max_chars: usize) -> String {
  value.chars().take(max_chars).collect()
}

fn push_unique<T: PartialEq>(items: &mut Vec<T>, item: T) {
  if !items.contains(&item) {
    items.push(item);
  }
}

fn upsert_artifact(artifacts: &mut Vec<CapsuleArtifact>, path: &str, note: String) {
  if let Some(artifact) = artifacts.iter_mut().find(|artifact| artifact.path == path) {
    artifact.note = note;
  } else {
    artifacts.push(CapsuleArtifact {
      path: path.into(),
      note,
    });
  }
}

fn retain_recent<T>(items: &mut Vec<T>, limit: usize) -> bool {
  if items.len() <= limit {
    return false;
  }
  items.drain(..items.len() - limit);
  true
}

fn contains_instruction_marker(line: &str) -> bool {
  let line = line.to_ascii_lowercase();
  [
    "must ", "should ", "do not ", "don't ", "never ", "avoid ", "require ",
  ]
  .iter()
  .any(|marker| line.contains(marker))
}

fn coding_command(call: &ToolCallBlock) -> Option<String> {
  call
    .arguments
    .get("command")
    .or_else(|| call.arguments.get("cmd"))
    .and_then(serde_json::Value::as_str)
    .map(str::to_string)
    .or_else(|| {
      call
        .arguments
        .get("argv")
        .and_then(serde_json::Value::as_array)
        .map(|argv| {
          argv
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect::<Vec<_>>()
            .join(" ")
        })
        .filter(|argv| !argv.is_empty())
    })
}

fn is_verification_command(command: &str) -> bool {
  let command = command.to_ascii_lowercase();
  [
    "test", "check", "clippy", "fmt", "pytest", "unittest", "vitest", "jest",
  ]
  .iter()
  .any(|marker| command.split_whitespace().any(|part| part.contains(marker)))
}

/// Synthesize a structured factual summary of older conversation messages.
pub fn structured_summary(messages: &[Message]) -> String {
  format_coding_summary(messages, None, "")
}

fn format_coding_summary(
  messages: &[Message],
  system: Option<&str>,
  current_state: &str,
) -> String {
  format!(
    "Summary of earlier conversation:\n{}",
    coding_capsule(messages, system, current_state).format_for_model()
  )
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::{Arc, Mutex};

  use crate::StoreTrace;
  use rupi_core::Tool;
  use rupi_core::{
    CompletionUsage, ProviderEvent, ProviderEventSink, SessionHeader, ThinkingLevel, ToolChunk,
    ToolMetadata, ToolOutcome, ToolPolicy, ToolRequest,
  };
  use rupi_tools::Workspace;

  /// One recorded event: its turn, its wire kind, and its payload.
  type Recorded = (Option<TurnId>, String, serde_json::Value);
  type Causal = (String, rupi_core::EventId, Option<rupi_core::EventId>);

  /// Events as they were emitted. Sequence numbers are stamped like a durable
  /// log so tests can assert on journal coordinates, not only on kinds.
  #[derive(Clone, Default)]
  struct Recorder(Arc<Mutex<Vec<Recorded>>>, Arc<Mutex<Vec<Causal>>>);

  impl Recorder {
    fn kinds(&self) -> Vec<String> {
      self
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|(_, kind, _)| kind.clone())
        .collect()
    }

    fn find(&self, kind: &str) -> Option<serde_json::Value> {
      self
        .0
        .lock()
        .unwrap()
        .iter()
        .find(|(_, k, _)| k == kind)
        .map(|(_, _, payload)| payload.clone())
    }

    /// Every diagnostic message, in order.
    fn diagnostics(&self) -> Vec<String> {
      self
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, kind, _)| kind == "diagnostic")
        .map(|(_, _, payload)| {
          payload
            .get("message")
            .and_then(|message| message.as_str())
            .unwrap_or_default()
            .to_string()
        })
        .collect()
    }

    fn count(&self, kind: &str) -> usize {
      self
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, k, _)| k == kind)
        .count()
    }
  }

  impl Trace for Recorder {
    fn emit(&mut self, envelope: &mut EventEnvelope) -> Result<(), SinkError> {
      // `AgentEvent` is internally tagged, so the discriminator is the `type`
      // field rather than a wrapper key. Reading any other key is what produced
      // payload field names instead of event names.
      let payload = serde_json::to_value(&envelope.event).unwrap_or(serde_json::Value::Null);
      let kind = payload
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or("?")
        .to_string();
      let mut recorded = self.0.lock().unwrap();
      envelope.meta.seq = Some(rupi_core::EventSeq(recorded.len() as u64 + 1));
      let event_id = envelope.meta.event_id.clone();
      let parent_event_id = envelope.meta.parent_event_id.clone();
      recorded.push((envelope.meta.turn_id.clone(), kind.clone(), payload));
      self
        .1
        .lock()
        .unwrap()
        .push((kind, event_id, parent_event_id));
      Ok(())
    }

    fn put_payload(&mut self, _bytes: &[u8]) -> Result<Option<BlobRef>, SinkError> {
      // No blob store in these tests: reduction must still be reported, which is
      // what the `None` path exercises.
      Ok(None)
    }
  }

  impl Recorder {
    /// The event identity and causal parent of every event of a kind.
    fn causal(&self, kind: &str) -> Vec<(rupi_core::EventId, Option<rupi_core::EventId>)> {
      self
        .1
        .lock()
        .unwrap()
        .iter()
        .filter(|(event_kind, _, _)| event_kind == kind)
        .map(|(_, event_id, parent)| (event_id.clone(), parent.clone()))
        .collect()
    }

    /// The payload of every event of a kind, in emission order.
    fn all(&self, kind: &str) -> Vec<serde_json::Value> {
      self
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, k, _)| k == kind)
        .map(|(_, _, payload)| payload.clone())
        .collect()
    }
  }

  /// A provider that replays scripted rounds, failing the configured ones.
  ///
  /// Each round is a list of events. `fail_rounds` are the 0-based request indices
  /// that return a failure instead of streaming.
  struct Scripted {
    model: ModelRef,
    capabilities: ModelCapabilities,
    rounds: Vec<Vec<ProviderEvent>>,
    fail: Vec<(usize, ModelFailure)>,
    calls: Arc<Mutex<Vec<(ModelRequest, ThinkingLevel, bool)>>>,
    /// Return `Ok` with an uncertain boundary instead of a usage report.
    unfinished: bool,
    /// Override the scripted response's provider finish reason.
    finish_reason: Option<String>,
    /// Supply provider usage independently of the scripted response content.
    completion_usage: Option<CompletionUsage>,
    /// Fail *every* request. `fail` is per-request-index and can run out, which
    /// cannot express a provider that is simply down.
    always: Option<ModelFailureKind>,
    /// Emit a fully normalized scripted round, then report a transport failure.
    fail_after_stream: Option<ModelFailureKind>,
  }

  impl Scripted {
    fn new(name: &str, rounds: Vec<Vec<ProviderEvent>>) -> Self {
      Self {
        model: ModelRef::new("test", name),
        capabilities: ModelCapabilities {
          text: true,
          images: false,
          tools: true,
          exposed_reasoning: rupi_core::ReasoningExposure::None,
          context_window: 128_000,
          max_output_tokens: Some(8_192),
        },
        rounds,
        fail: Vec::new(),
        calls: Arc::new(Mutex::new(Vec::new())),
        unfinished: false,
        finish_reason: None,
        completion_usage: None,
        always: None,
        fail_after_stream: None,
      }
    }

    fn fails(mut self, index: usize, failure: ModelFailure) -> Self {
      self.fail.push((index, failure));
      self
    }

    /// Fail *every* request with `kind`, however often the loop retries.
    fn always_fails(mut self, kind: ModelFailureKind) -> Self {
      self.always = Some(kind);
      self
    }

    fn fails_after_stream(mut self, kind: ModelFailureKind) -> Self {
      self.fail_after_stream = Some(kind);
      self
    }

    fn finishes_with(mut self, reason: &str) -> Self {
      self.finish_reason = Some(reason.into());
      self
    }

    fn with_usage(mut self, usage: CompletionUsage) -> Self {
      self.completion_usage = Some(usage);
      self
    }

    /// A provider whose first response ends without any completion signal.
    ///
    /// Stands in for a connection that closed mid-sentence: the transport did not
    /// error, and only the completion accounting distinguishes this from an answer.
    fn uncertain(name: &str, partial: &str) -> Self {
      let mut scripted = Self::new(name, vec![vec![ProviderEvent::TextDelta(partial.into())]]);
      scripted.unfinished = true;
      scripted
    }

    fn requests(&self) -> Vec<ModelRequest> {
      self
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|(request, _, _)| request.clone())
        .collect()
    }

    fn levels(&self) -> Vec<ThinkingLevel> {
      self
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|(_, level, _)| *level)
        .collect()
    }
  }

  impl ModelProvider for Scripted {
    fn provider_id(&self) -> &str {
      "test"
    }

    fn model(&self) -> &ModelRef {
      &self.model
    }

    fn capabilities(&self) -> ModelCapabilities {
      self.capabilities.clone()
    }

    #[allow(clippy::result_large_err)]
    fn stream(
      &self,
      request: &ModelRequest,
      sink: &mut dyn ProviderEventSink,
      _cancel: &CancelToken,
    ) -> Result<CompletionUsage, ModelFailure> {
      // The third field records whether a request was answered. Only answered
      // requests advance the script, so `fails(0, ..)` means "the first attempt
      // fails" rather than "the first round is skipped forever".
      // A scripted failure stands in for a provider that never answered. The request
      // is recorded as *not* answered, so it consumes no round and the next request
      // replays the round this one was meant to get. That is what makes
      // `fails(0, ..)` mean "the first attempt fails" rather than "the first round is
      // skipped forever".
      let (request_index, served, injects) = {
        let mut calls = self.calls.lock().unwrap();
        let index = calls.len();
        let injects = self.always.is_some() || self.fail.iter().any(|(at, _)| *at == index);
        // Requests that never answered are not rounds, so they do not advance the
        // script: `fails(0, ..)` means "the first attempt fails" and the retry gets
        // the same round, not the next one.
        let answered = calls.iter().filter(|call| call.2).count();
        calls.push((request.clone(), request.thinking, !injects));
        (index, answered, injects)
      };
      if injects {
        if let Some(kind) = self.always {
          let mut failure =
            ModelFailure::new(kind, FailurePhase::WaitingForResponse, "provider is down");
          failure.model = Some(self.model.clone());
          return Err(failure);
        }
        let (_, failure) = self
          .fail
          .iter()
          .find(|(at, _)| *at == request_index)
          .expect("injects");
        return Err(clone_failure(failure));
      }
      if self.unfinished {
        // The stream produced whatever it scripted and then simply stopped: exactly
        // the shape a disconnected socket leaves behind.
        for event in self.rounds.first().into_iter().flatten() {
          sink.emit(event);
        }
        return Ok(CompletionUsage::unfinished());
      }
      let usage = if let Some(events) = self.rounds.get(served) {
        let mut usage = CompletionUsage::unknown();
        if events.is_empty() {
          usage.finish_reason = Some("stop".into());
        }
        for event in events {
          match event {
            ProviderEvent::TextDelta(_) => usage.output_tokens = Some(4),
            ProviderEvent::ToolCall(_) | ProviderEvent::ToolCallRejected { .. } => {
              usage.finish_reason = Some("tool_calls".into())
            }
            ProviderEvent::ReasoningDelta { .. } => {}
          }
          sink.emit(event);
        }
        if let Some(usage) = &self.completion_usage {
          return Ok(usage.clone());
        }
        if let Some(reason) = &self.finish_reason {
          usage.finish_reason = Some(reason.clone());
        }
        if let Some(kind) = self.fail_after_stream {
          return Err(ModelFailure::new(
            kind,
            FailurePhase::Streaming,
            "stream failed after decoded output",
          ));
        }
        usage
      } else {
        // A script that ran out is a bug in the test, not a provider behavior.
        return Err(ModelFailure::new(
          ModelFailureKind::Protocol,
          FailurePhase::Streaming,
          "script exhausted",
        ));
      };
      Ok(usage)
    }
  }

  /// Model failures are not `Clone` on purpose; tests need copies.
  fn clone_failure(failure: &ModelFailure) -> ModelFailure {
    let mut copy = ModelFailure::new(failure.kind, failure.phase, failure.message.clone());
    copy.retry_after_ms = failure.retry_after_ms;
    copy.status = failure.status;
    copy.partial_output_emitted = failure.partial_output_emitted;
    copy.attempts = failure.attempts;
    copy.model = failure.model.clone();
    copy
  }

  fn text(value: &str) -> Vec<ProviderEvent> {
    vec![ProviderEvent::TextDelta(value.into())]
  }

  fn tool_call(name: &str, arguments: serde_json::Value) -> Vec<ProviderEvent> {
    vec![ProviderEvent::ToolCall(ToolCallBlock {
      id: rupi_core::ToolCallId::new(),
      name: name.into(),
      arguments,
    })]
  }

  fn tool_message_result(
    id: rupi_core::ToolCallId,
    state: ToolExecutionState,
    text: &str,
  ) -> Message {
    Message::new(
      Role::Tool,
      vec![ContentBlock::ToolResult(ToolResultBlock {
        id,
        name: "test_tool".into(),
        state,
        text: text.into(),
        is_error: state == ToolExecutionState::Failed,
        reduced: false,
      })],
    )
  }

  #[test]
  fn coding_summary_preserves_objective_artifacts_and_verification_outcomes() {
    let edit_id = rupi_core::ToolCallId::new();
    let passed_id = rupi_core::ToolCallId::new();
    let failed_id = rupi_core::ToolCallId::new();
    let unknown_id = rupi_core::ToolCallId::new();
    let pending_id = rupi_core::ToolCallId::new();
    let messages = vec![
      Message::user("Build the parser.\nMust retain the existing input format."),
      Message::new(
        Role::Assistant,
        vec![ContentBlock::ToolCall(ToolCallBlock {
          id: edit_id.clone(),
          name: "edit".into(),
          arguments: serde_json::json!({"path":"src/parser.rs"}),
        })],
      ),
      tool_message_result(edit_id, ToolExecutionState::Succeeded, "updated file"),
      Message::new(
        Role::Assistant,
        vec![ContentBlock::ToolCall(ToolCallBlock {
          id: passed_id.clone(),
          name: "exec".into(),
          arguments: serde_json::json!({"command":"cargo test -p parser"}),
        })],
      ),
      tool_message_result(passed_id, ToolExecutionState::Succeeded, "test result: ok"),
      Message::new(
        Role::Assistant,
        vec![ContentBlock::ToolCall(ToolCallBlock {
          id: failed_id.clone(),
          name: "exec".into(),
          arguments: serde_json::json!({"command":"cargo clippy -p parser"}),
        })],
      ),
      tool_message_result(
        failed_id,
        ToolExecutionState::Failed,
        "warning denied by lint",
      ),
      Message::new(
        Role::Assistant,
        vec![ContentBlock::ToolCall(ToolCallBlock {
          id: unknown_id.clone(),
          name: "write".into(),
          arguments: serde_json::json!({"path":"src/possibly-written.rs"}),
        })],
      ),
      tool_message_result(
        unknown_id,
        ToolExecutionState::Unknown,
        "connection ended while writing",
      ),
      Message::new(
        Role::Assistant,
        vec![ContentBlock::ToolCall(ToolCallBlock {
          id: pending_id,
          name: "edit".into(),
          arguments: serde_json::json!({"path":"src/interrupted.rs"}),
        })],
      ),
    ];

    let summary = structured_summary(&messages);
    assert!(
      summary.contains("objective: Build the parser."),
      "{summary}"
    );
    assert!(
      summary.contains("User instruction: Must retain the existing input format."),
      "{summary}"
    );
    assert!(
      summary.contains("src/parser.rs: modified by `edit`"),
      "{summary}"
    );
    assert!(
      summary.contains("Verification passed: `cargo test -p parser`"),
      "{summary}"
    );
    assert!(
      summary.contains("Verification failed: `cargo clippy -p parser`"),
      "{summary}"
    );
    assert!(
      summary.contains("Tool `write` for `src/possibly-written.rs` ended in state `unknown`"),
      "{summary}"
    );
    assert!(
      summary.contains("visible result: connection ended while writing"),
      "{summary}"
    );
    assert!(
      summary.contains("Tool call `edit` for `src/interrupted.rs` has no visible terminal result"),
      "{summary}"
    );
    assert!(!summary.contains("Tool `write` completed"), "{summary}");
  }

  #[test]
  fn coding_summary_carries_progress_forward_from_an_earlier_capsule() {
    let mut prior = ContextCapsule::new("Build the parser");
    prior
      .completed_work
      .push("lexer implementation completed".into());
    prior.artifacts.push(CapsuleArtifact {
      path: r"C:\repo\src\lexer.rs".into(),
      note: "modified by `edit`".into(),
    });
    prior
      .unresolved
      .push("integration tests have not run".into());
    prior.next_actions.push("run integration tests".into());
    let messages = vec![Message::user(prior.format_for_model())];

    let summary = structured_summary(&messages);
    assert!(summary.contains("objective: Build the parser"), "{summary}");
    assert!(
      summary.contains("lexer implementation completed"),
      "{summary}"
    );
    assert!(summary.contains(r"C:\repo\src\lexer.rs"), "{summary}");
    assert!(
      summary.contains("integration tests have not run"),
      "{summary}"
    );
    assert!(summary.contains("run integration tests"), "{summary}");
  }

  #[test]
  fn completed_cycle_boundary_requires_all_known_terminal_results() {
    let first = rupi_core::ToolCallId::new();
    let second = rupi_core::ToolCallId::new();
    let calls = Message::new(
      Role::Assistant,
      vec![
        ContentBlock::ToolCall(ToolCallBlock {
          id: first.clone(),
          name: "read".into(),
          arguments: serde_json::json!({}),
        }),
        ContentBlock::ToolCall(ToolCallBlock {
          id: second.clone(),
          name: "read".into(),
          arguments: serde_json::json!({}),
        }),
      ],
    );
    let messages = vec![
      calls,
      tool_message_result(first, ToolExecutionState::Succeeded, "first"),
      tool_message_result(second.clone(), ToolExecutionState::Succeeded, "second"),
    ];
    assert!(!safe_completed_cycle_boundary(&messages, 0, 2));
    assert!(safe_completed_cycle_boundary(&messages, 0, 3));

    let uncertain = vec![
      messages[0].clone(),
      tool_message_result(second, ToolExecutionState::Unknown, "uncertain"),
    ];
    assert!(!safe_completed_cycle_boundary(&uncertain, 0, 2));
  }

  #[test]
  fn completed_cycle_boundary_does_not_cross_an_earlier_unknown_result() {
    let completed = rupi_core::ToolCallId::new();
    let uncertain = rupi_core::ToolCallId::new();
    let later = rupi_core::ToolCallId::new();
    let call = |id: &rupi_core::ToolCallId| {
      Message::new(
        Role::Assistant,
        vec![ContentBlock::ToolCall(ToolCallBlock {
          id: id.clone(),
          name: "read".into(),
          arguments: serde_json::json!({}),
        })],
      )
    };
    let messages = vec![
      call(&completed),
      tool_message_result(completed, ToolExecutionState::Succeeded, "completed"),
      call(&uncertain),
      tool_message_result(uncertain, ToolExecutionState::Unknown, "outcome unknown"),
      call(&later),
      tool_message_result(later, ToolExecutionState::Succeeded, "later completed"),
    ];

    assert!(safe_completed_cycle_boundary(&messages, 0, 2));
    assert!(!safe_completed_cycle_boundary(&messages, 0, 4));
    assert!(safe_completed_cycle_boundary(&messages, 4, 6));
    assert!(!safe_completed_cycle_boundary(&messages, 0, 6));
  }

  #[test]
  fn completed_cycles_compact_inside_one_user_turn_and_keep_whole_recent_pairs() {
    let mut messages = vec![Message::user(
      "Implement the parser and preserve its format.",
    )];
    for index in 0..24 {
      let id = rupi_core::ToolCallId::new();
      messages.push(Message::new(
        Role::Assistant,
        vec![ContentBlock::ToolCall(ToolCallBlock {
          id: id.clone(),
          name: "edit".into(),
          arguments: serde_json::json!({"path":format!("src/module_{index}.rs")}),
        })],
      ));
      messages.push(tool_message_result(
        id,
        ToolExecutionState::Succeeded,
        &"file update observed ".repeat(40),
      ));
    }
    let original_len = messages.len();
    let provider = Scripted::new("cycle-compaction", vec![text("unused")]);
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let mut trace = Recorder::default();
    let mut runtime = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_messages(messages);
    let turn_id = TurnId::new();
    let mut turn_history_start = 1;

    let replaced = runtime
      .compact_completed_turn_cycles(
        &turn_id,
        &mut turn_history_start,
        3_000,
        "test pressure".into(),
      )
      .expect("completed cycles compact");

    assert!(replaced > 0);
    assert!(runtime.messages().len() < original_len);
    let summary = runtime
      .messages()
      .iter()
      .find(|message| message.text().contains("[Session Checkpoint Capsule]"))
      .expect("structured summary remains visible")
      .text();
    assert!(
      summary.contains("objective: Implement the parser"),
      "{summary}"
    );
    assert!(summary.contains("src/module_0.rs"), "{summary}");
    let call_ids: BTreeSet<_> = runtime
      .messages()
      .iter()
      .filter(|message| message.role == Role::Assistant)
      .flat_map(Message::tool_calls)
      .map(|call| call.id.to_string())
      .collect();
    let result_ids: BTreeSet<_> = runtime
      .messages()
      .iter()
      .filter(|message| message.role == Role::Tool)
      .flat_map(|message| &message.content)
      .filter_map(|block| match block {
        ContentBlock::ToolResult(result) => Some(result.id.to_string()),
        _ => None,
      })
      .collect();
    assert_eq!(
      call_ids, result_ids,
      "retained tool calls and results stay paired"
    );
    assert_eq!(trace.count("context_compaction_epoch"), 1);
  }

  /// A tool that records what it was asked to do.
  #[derive(Clone)]
  struct Spy(Arc<Mutex<Vec<serde_json::Value>>>);

  impl Tool for Spy {
    fn metadata(&self) -> ToolMetadata {
      ToolMetadata::read_only("spy", "records its arguments")
    }

    fn arguments_schema(&self) -> serde_json::Value {
      serde_json::json!({"type":"object"})
    }

    #[allow(clippy::result_large_err)]
    fn execute(
      &self,
      request: &ToolRequest,
      progress: &mut dyn rupi_core::ToolProgress,
    ) -> Result<ToolOutcome, rupi_core::ToolError> {
      progress.emit(&ToolChunk::new("working"));
      self.0.lock().unwrap().push(request.arguments.clone());
      Ok(ToolOutcome::succeeded("noted"))
    }
  }

  struct CancelFirstCall;

  impl Tool for CancelFirstCall {
    fn metadata(&self) -> ToolMetadata {
      ToolMetadata::read_only("cancel_first", "cancels the turn after this call")
    }

    fn arguments_schema(&self) -> serde_json::Value {
      serde_json::json!({"type":"object"})
    }

    fn execute(
      &self,
      _request: &ToolRequest,
      _progress: &mut dyn rupi_core::ToolProgress,
    ) -> Result<ToolOutcome, rupi_core::ToolError> {
      Ok(ToolOutcome::succeeded("cancelled"))
    }

    fn execute_with_context(
      &self,
      _request: &ToolRequest,
      _progress: &mut dyn rupi_core::ToolProgress,
      context: &rupi_core::ToolExecutionContext,
    ) -> Result<ToolOutcome, rupi_core::ToolError> {
      context.cancel_token().cancel();
      Ok(ToolOutcome::succeeded("cancelled"))
    }
  }

  /// A tool that represents the requested mutation for the progress-boundary
  /// test without touching the host filesystem.
  #[derive(Clone)]
  struct MutatingSpy {
    seen: Arc<Mutex<Vec<serde_json::Value>>>,
    outcome: ToolOutcome,
  }

  impl Tool for MutatingSpy {
    fn metadata(&self) -> ToolMetadata {
      ToolMetadata::mutating("write_probe", "records a requested mutation", true)
    }

    fn arguments_schema(&self) -> serde_json::Value {
      serde_json::json!({"type":"object"})
    }

    #[allow(clippy::result_large_err)]
    fn execute(
      &self,
      request: &ToolRequest,
      _progress: &mut dyn rupi_core::ToolProgress,
    ) -> Result<ToolOutcome, rupi_core::ToolError> {
      self.seen.lock().unwrap().push(request.arguments.clone());
      Ok(self.outcome.clone())
    }
  }

  struct SizedTool {
    description: String,
    schema: serde_json::Value,
  }

  impl Tool for SizedTool {
    fn metadata(&self) -> ToolMetadata {
      ToolMetadata::read_only("sized", self.description.clone())
    }

    fn arguments_schema(&self) -> serde_json::Value {
      self.schema.clone()
    }

    #[allow(clippy::result_large_err)]
    fn execute(
      &self,
      _request: &ToolRequest,
      _progress: &mut dyn rupi_core::ToolProgress,
    ) -> Result<ToolOutcome, rupi_core::ToolError> {
      Ok(ToolOutcome::succeeded("ok"))
    }
  }

  /// A registry over a throwaway workspace, with `tools` auto-approved.
  fn registry_with(tools: Vec<Box<dyn Tool>>) -> ToolRegistry {
    let policy = rupi_core::ToolPolicy {
      auto_approve_mutating: true,
      ..Default::default()
    };
    let mut registry = ToolRegistry::new(Workspace::new(std::env::temp_dir()).expect("temp dir"))
      .with_policy(&policy);
    for tool in tools {
      registry.register(tool);
    }
    registry
  }

  fn registry_with_default_deny(tools: Vec<Box<dyn Tool>>) -> ToolRegistry {
    let mut registry = ToolRegistry::new(Workspace::new(std::env::temp_dir()).expect("temp dir"));
    for tool in tools {
      registry.register(tool);
    }
    registry
  }

  struct ApprovalProgress {
    decision: Approval,
    prompts: usize,
  }

  impl TurnProgress for ApprovalProgress {
    fn approve_mutating_tool(
      &mut self,
      _metadata: &rupi_core::ToolMetadata,
      _arguments: &serde_json::Value,
    ) -> Approval {
      self.prompts += 1;
      self.decision.clone()
    }
  }

  #[test]
  fn model_tools_and_guidance_match_headless_and_interactive_approval() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tools = registry_with_default_deny(vec![
      Box::new(Spy(Arc::new(Mutex::new(Vec::new())))),
      Box::new(MutatingSpy {
        seen,
        outcome: ToolOutcome::succeeded("ok"),
      }),
    ]);
    let provider = Scripted::new("tool-affordances", Vec::new());
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );

    let mut trace = Recorder::default();
    let mut headless = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_messages(vec![Message::user("inspect the workspace")]);
    let mut turn_history_start = 0;
    let headless_request = headless
      .build_request(&TurnId::new(), &mut turn_history_start)
      .expect("headless request builds");
    let headless_names: Vec<_> = headless_request
      .tools
      .iter()
      .map(|spec| spec.name.as_str())
      .collect();
    assert_eq!(headless_names, ["spy"]);
    let headless_system = headless_request.system.as_deref().unwrap();
    assert!(headless_system.contains("Tools available for this request: spy"));
    assert!(!headless_system.contains("write_probe"));

    let mut trace = Recorder::default();
    let mut interactive = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_interactive_tool_approval(true)
    .with_messages(vec![Message::user("inspect the workspace")]);
    let mut turn_history_start = 0;
    let interactive_request = interactive
      .build_request(&TurnId::new(), &mut turn_history_start)
      .expect("interactive request builds");
    let interactive_system = interactive_request.system.as_deref().unwrap();
    assert!(interactive_system.contains("spy, write_probe"));

    let mut without_tool_support = Scripted::new("no-tools", Vec::new());
    without_tool_support.capabilities.tools = false;
    let mut trace = Recorder::default();
    let mut degraded = TurnLoop::new(
      &without_tool_support,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_interactive_tool_approval(true);
    let mut turn_history_start = 0;
    let degraded_request = degraded
      .build_request(&TurnId::new(), &mut turn_history_start)
      .expect("tool-less model request builds");
    assert!(degraded_request.tools.is_empty());
    assert!(
      degraded_request
        .system
        .as_deref()
        .unwrap()
        .contains("No tools are available for this request.")
    );
  }

  #[test]
  fn default_headless_refuses_mutation_and_interactive_approval_executes_it() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tools = registry_with_default_deny(vec![Box::new(MutatingSpy {
      seen: Arc::clone(&seen),
      outcome: ToolOutcome::succeeded("mutated"),
    })]);
    let policy = rupi_core::ProfilePolicy::new(rupi_core::ContextProfile::Balanced, 128_000);

    let provider = Scripted::new(
      "headless-denial",
      vec![
        tool_call("write_probe", serde_json::json!({})),
        text("done"),
      ],
    );
    let mut trace = Recorder::default();
    TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .run_turn(
      "change the workspace",
      &CancelToken::new(),
      &mut SilentProgress,
    )
    .expect("a refused tool call still has a model-visible result");
    assert!(seen.lock().unwrap().is_empty());

    let provider = Scripted::new(
      "interactive-approval",
      vec![
        tool_call("write_probe", serde_json::json!({})),
        text("done"),
      ],
    );
    let mut trace = Recorder::default();
    let mut progress = ApprovalProgress {
      decision: Approval::Allow,
      prompts: 0,
    };
    TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_interactive_tool_approval(true)
    .run_turn("change the workspace", &CancelToken::new(), &mut progress)
    .expect("approved tool call completes");
    assert_eq!(progress.prompts, 1);
    assert_eq!(seen.lock().unwrap().len(), 1);
  }

  struct AlwaysReduce;

  impl ContextPolicy for AlwaysReduce {
    fn evaluate(&self, state: &ContextState) -> rupi_core::ContextDecision {
      rupi_core::ContextDecision {
        action: ContextAction::ReducePayload {
          reason: ReductionReason::RecentTargetExceeded { target_tokens: 1 },
        },
        tokens: state.effective_tokens(),
        level: Some(ContextLevel::L0Payload),
      }
    }

    fn name(&self) -> &'static str {
      "test_reduce"
    }
  }

  struct CaptureContextState(Arc<Mutex<Option<ContextState>>>);

  impl ContextPolicy for CaptureContextState {
    fn evaluate(&self, state: &ContextState) -> rupi_core::ContextDecision {
      *self.0.lock().unwrap() = Some(state.clone());
      rupi_core::ContextDecision {
        action: ContextAction::Keep,
        tokens: state.effective_tokens(),
        level: None,
      }
    }

    fn name(&self) -> &'static str {
      "capture_context_state"
    }
  }

  #[test]
  fn pre_policy_estimate_includes_system_prompt_and_tool_schemas() {
    let mut provider = Scripted::new("request-sizing", Vec::new());
    provider.capabilities.context_window = 16_000;
    let tools = registry_with(vec![Box::new(SizedTool {
      description: "description ".repeat(4_000),
      schema: serde_json::json!({"type":"object","description":"schema ".repeat(4_000)}),
    })]);
    let observed = Arc::new(Mutex::new(None));
    let policy = CaptureContextState(Arc::clone(&observed));
    let mut trace = Recorder::default();
    let mut runtime = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_system("system ".repeat(4_000))
    .with_messages(vec![Message::user("short request")]);
    let mut turn_history_start = 0;

    let request = runtime
      .build_request(&TurnId::new(), &mut turn_history_start)
      .expect("request is assembled");
    let state = observed
      .lock()
      .unwrap()
      .clone()
      .expect("policy receives context state");

    assert_eq!(state.estimated_tokens, estimate_tokens(&request));
    assert!(state.estimated_tokens > estimate_messages(&request.messages));
    assert!(
      state.estimated_tokens
        > rupi_core::ContextThresholds::for_profile(
          rupi_core::ContextProfile::Balanced,
          provider.capabilities.context_window,
        )
        .compact_tokens,
      "system text and tool schema alone should push this small window past its working threshold"
    );
    assert!(!request.tools.is_empty());
    assert!(request.system.is_some());
  }

  struct RecordingProfilePolicy {
    profile: rupi_core::ProfilePolicy,
    observations: Arc<Mutex<Vec<(ContextState, rupi_core::ContextDecision)>>>,
  }

  impl ContextPolicy for RecordingProfilePolicy {
    fn evaluate(&self, state: &ContextState) -> rupi_core::ContextDecision {
      let decision = self.profile.evaluate(state);
      self
        .observations
        .lock()
        .unwrap()
        .push((state.clone(), decision.clone()));
      decision
    }

    fn name(&self) -> &'static str {
      self.profile.name()
    }
  }

  struct ExpandedResultTool(String);

  impl Tool for ExpandedResultTool {
    fn metadata(&self) -> ToolMetadata {
      ToolMetadata::read_only("expand", "returns a bounded fixture result")
    }

    fn arguments_schema(&self) -> serde_json::Value {
      serde_json::json!({"type":"object"})
    }

    fn execute(
      &self,
      _request: &ToolRequest,
      _progress: &mut dyn rupi_core::ToolProgress,
    ) -> Result<ToolOutcome, rupi_core::ToolError> {
      Ok(ToolOutcome::succeeded(self.0.clone()))
    }
  }

  #[test]
  fn context_policy_uses_the_current_request_after_a_large_tool_result() {
    let mut provider = Scripted::new(
      "current-context-sizing",
      vec![tool_call("expand", serde_json::json!({})), text("done")],
    );
    provider.capabilities.context_window = 32_000;
    let mut usage = rupi_core::CompletionUsage::unknown();
    usage.input_tokens = Some(8_000);
    usage.logical_prompt_tokens = Some(8_000);
    let provider = provider.with_usage(usage);

    let tool_policy = rupi_core::ToolPolicy {
      auto_approve_mutating: true,
      max_output_bytes: 160_000,
      ..Default::default()
    };
    let mut tools = ToolRegistry::new(Workspace::new(std::env::temp_dir()).expect("temp dir"))
      .with_policy(&tool_policy);
    tools.register(Box::new(ExpandedResultTool("x".repeat(100_000))));

    let observations = Arc::new(Mutex::new(Vec::new()));
    let policy = RecordingProfilePolicy {
      profile: rupi_core::ProfilePolicy::new(rupi_core::ContextProfile::Balanced, 32_000),
      observations: Arc::clone(&observations),
    };
    let mut trace = Recorder::default();
    let report = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .run_turn("expand context", &CancelToken::new(), &mut SilentProgress)
    .expect("turn completes after reducing the completed tool cycle");

    assert_eq!(report.status, TurnStatus::Completed);
    let observations = observations.lock().unwrap();
    assert!(observations.len() >= 2);
    let (second_state, second_decision) = &observations[1];
    assert_eq!(second_state.measured_tokens, None);
    assert!(second_state.estimated_tokens > 24_000);
    assert!(matches!(
      &second_decision.action,
      ContextAction::Compact { .. }
    ));
    assert!(
      trace
        .kinds()
        .iter()
        .any(|kind| kind == "context_compaction_started")
    );
    assert!(
      estimate_tokens(&provider.requests()[1]) < second_state.estimated_tokens,
      "the measured current request is reduced before dispatch"
    );
  }

  #[test]
  fn reduce_payload_compacts_completed_work_before_the_next_model_request() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tools = registry_with(vec![Box::new(Spy(Arc::clone(&seen)))]);
    let provider = Scripted::new(
      "reduce-completed-cycle",
      vec![
        tool_call("spy", serde_json::json!({"path":"src/parser.rs"})),
        text("continue from the recorded result"),
      ],
    );
    let mut trace = Recorder::default();
    let report = TurnLoop::new(
      &provider,
      &tools,
      &AlwaysReduce,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .run_turn("Build the parser", &CancelToken::new(), &mut SilentProgress)
    .expect("turn completes after reducing the first cycle");

    assert_eq!(report.status, TurnStatus::Completed);
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(seen.lock().unwrap().len(), 1);
    let next_request = &provider.requests()[1];
    let summary = next_request
      .messages
      .iter()
      .find(|message| message.text().contains("[Session Checkpoint Capsule]"))
      .expect("the next request carries a coding capsule")
      .text();
    assert!(summary.contains("objective: Build the parser"), "{summary}");
    assert!(summary.contains("src/parser.rs"), "{summary}");
    assert!(summary.contains("Tool `spy` completed"), "{summary}");
    assert_eq!(trace.count("context_compaction_epoch"), 1);
  }

  struct FailingMessageSink {
    events: usize,
  }

  impl Trace for FailingMessageSink {
    fn emit(&mut self, _envelope: &mut EventEnvelope) -> Result<(), SinkError> {
      self.events += 1;
      Ok(())
    }

    fn record_message(&mut self, _attributed: &AttributedMessage) -> Result<(), SinkError> {
      Err(SinkError("session log is unavailable".into()))
    }
  }

  struct FailingCompactionSink {
    summary_event: bool,
  }

  impl Trace for FailingCompactionSink {
    fn emit(&mut self, envelope: &mut EventEnvelope) -> Result<(), SinkError> {
      if matches!(envelope.event, AgentEvent::ContextSummary) {
        self.summary_event = true;
      }
      Ok(())
    }

    fn record_message(&mut self, _attributed: &AttributedMessage) -> Result<(), SinkError> {
      if self.summary_event {
        return Err(SinkError(
          "compaction summary could not be persisted".into(),
        ));
      }
      Ok(())
    }
  }

  #[test]
  fn a_message_sink_failure_stops_before_the_provider_request() {
    let provider = Scripted::new("unused", vec![text("must not run")]);
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let mut trace = FailingMessageSink { events: 0 };
    let error = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .run_turn("hello", &CancelToken::new(), &mut SilentProgress)
    .unwrap_err();

    assert!(matches!(error, TurnError::Sink(message) if message.contains("session log")));
    assert!(
      provider.requests().is_empty(),
      "sink failure must be terminal"
    );
    assert_eq!(trace.events, 3, "session, epoch, then user event");
  }

  struct FailingSecondDelta {
    kinds: Vec<String>,
    deltas: usize,
  }

  impl Trace for FailingSecondDelta {
    fn emit(&mut self, envelope: &mut EventEnvelope) -> Result<(), SinkError> {
      let kind = serde_json::to_value(&envelope.event)
        .ok()
        .and_then(|value| value["type"].as_str().map(str::to_string))
        .unwrap_or_default();
      if kind == "assistant_delta" {
        self.deltas += 1;
        if self.deltas == 2 {
          return Err(SinkError("trace delta write failed".into()));
        }
      }
      self.kinds.push(kind);
      Ok(())
    }
  }

  #[derive(Default)]
  struct TextSpy(Vec<String>);

  impl TurnProgress for TextSpy {
    fn on_text_delta(&mut self, text: &str) {
      self.0.push(text.to_string());
    }
  }

  #[test]
  fn provider_delta_sink_failure_is_retained_and_cancels_streaming() {
    let provider = Scripted::new(
      "two-deltas",
      vec![vec![
        ProviderEvent::TextDelta("first".into()),
        ProviderEvent::TextDelta("second".into()),
      ]],
    );
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let mut trace = FailingSecondDelta {
      kinds: Vec::new(),
      deltas: 0,
    };
    let mut progress = TextSpy::default();
    let cancel = CancelToken::new();

    let error = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .run_turn("hello", &cancel, &mut progress)
    .unwrap_err();

    assert!(matches!(error, TurnError::Sink(message) if message.contains("delta write")));
    assert!(cancel.is_cancelled());
    assert_eq!(progress.0, vec!["first"]);
    assert_eq!(trace.deltas, 2);
    assert_eq!(
      trace
        .kinds
        .iter()
        .filter(|kind| kind.as_str() == "assistant_delta")
        .count(),
      1
    );
  }

  #[test]
  fn an_empty_completed_response_commits_without_a_message_projection() {
    let temp = rupi_store::TempDir::new("runtime-empty-completion");
    let store = rupi_store::Store::open(temp.path(), rupi_store::WritePolicy::default())
      .expect("store opens");
    let session_id = SessionId::new();
    let provider = Scripted::new("empty", vec![Vec::new()]);
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let session = store
      .begin(SessionHeader {
        session_id: session_id.clone(),
        version: rupi_core::session::SESSION_SCHEMA_VERSION,
        started_at_ms: 1,
        working_dir: "/workspace".into(),
        model: provider.model().clone(),
        parent_session: None,
        branched_from_event: None,
        imported_from: None,
      })
      .unwrap();
    let mut trace = StoreTrace::new(session);
    let mut runtime = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      session_id.clone(),
      TraceId::new(),
    );
    runtime
      .run_turn("empty answer", &CancelToken::new(), &mut SilentProgress)
      .expect("an explicit stop with no content is still a completed response");
    drop(runtime);
    trace.flush().unwrap();
    drop(trace);
    store
      .restore(&session_id)
      .expect("empty completion WAL is committed");
  }

  #[test]
  fn a_plain_answer_emits_the_canonical_turn_shape() {
    let provider = Scripted::new("capable", vec![text("done")]);
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let mut trace = Recorder::default();
    let mut harness = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_system("be brief");

    let report = harness
      .run_turn("hello", &CancelToken::new(), &mut SilentProgress)
      .unwrap();

    assert_eq!(report.text, "done");
    assert_eq!(report.status, TurnStatus::Completed);
    assert_eq!(report.requests, 1);
    assert_eq!(report.tool_calls, 0);
    assert!(!report.budget_exhausted);

    let kinds = trace.kinds();
    for expected in [
      "session_started",
      "model_epoch_started",
      "user_message",
      "model_request_started",
      "model_request_completed",
      "turn_completed",
    ] {
      assert!(
        kinds.iter().any(|k| k == expected),
        "missing {expected} in {kinds:?}"
      );
    }
    // Order matters more than presence: a trace that cannot answer "did the request
    // start before it completed" is not a trace.
    let started = kinds
      .iter()
      .position(|k| k == "model_request_started")
      .unwrap();
    let delta = kinds.iter().position(|k| k == "assistant_delta").unwrap();
    let completed = kinds
      .iter()
      .position(|k| k == "model_request_completed")
      .unwrap();
    assert!(started < delta && delta < completed);
    // The turn owns its events, which is what makes a per-turn query possible.
    assert!(trace.0.lock().unwrap().iter().all(|(turn, kind, _)| {
      matches!(kind.as_str(), "session_started" | "model_epoch_started") || turn.is_some()
    }));
    // The system prompt reached the provider, and only once.
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].system.as_deref().is_some_and(|system| {
      system.starts_with("be brief\n\nNo tools are available for this request.")
    }));
    assert_eq!(
      requests[0].messages.len(),
      1,
      "the user turn is the only history a first turn has"
    );
  }

  #[test]
  fn provider_overflow_compacts_old_history_and_reissues_once() {
    let provider = Scripted::new("overflow", vec![text("recovered")]).fails(
      0,
      ModelFailure::new(
        ModelFailureKind::ContextOverflow,
        FailurePhase::WaitingForResponse,
        "context window exceeded",
      ),
    );
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let mut trace = Recorder::default();
    let mut runtime = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_messages(vec![Message::user("old history")]);

    let report = runtime
      .run_turn("new turn", &CancelToken::new(), &mut SilentProgress)
      .expect("the compacted reissue succeeds");

    assert_eq!(report.text, "recovered");
    assert_eq!(report.requests, 2);
    assert_eq!(trace.count("context_compaction_epoch"), 1);
    assert_eq!(trace.count("model_failover"), 0);
    assert_eq!(trace.count("model_request_started"), 2);
    assert_eq!(trace.count("model_request_completed"), 2);
    let reissue = &provider.requests()[1];
    assert!(
      reissue.messages[0]
        .text()
        .contains("Summary of earlier conversation")
    );
    assert_eq!(reissue.messages[1].text(), "new turn");
  }

  #[test]
  fn a_second_provider_overflow_is_terminal_after_one_compaction() {
    let overflow = ModelFailure::new(
      ModelFailureKind::ContextOverflow,
      FailurePhase::WaitingForResponse,
      "context window exceeded",
    );
    let provider = Scripted::new("overflow-twice", Vec::new())
      .fails(0, overflow.clone())
      .fails(1, overflow);
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let mut trace = Recorder::default();
    let error = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_messages(vec![Message::user("old history")])
    .run_turn("new turn", &CancelToken::new(), &mut SilentProgress)
    .expect_err("the second refusal is terminal");

    assert_eq!(error.kind(), Some(ModelFailureKind::ContextOverflow));
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(trace.count("context_compaction_epoch"), 1);
    assert_eq!(trace.count("model_request_started"), 2);
    assert_eq!(trace.count("model_request_completed"), 2);
  }

  #[test]
  fn text_before_context_overflow_is_not_replayed() {
    let provider = Scripted::new("partial-overflow", vec![text("partial")])
      .fails_after_stream(ModelFailureKind::ContextOverflow);
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let mut trace = Recorder::default();
    let error = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_messages(vec![Message::user("old history")])
    .run_turn("new turn", &CancelToken::new(), &mut SilentProgress)
    .expect_err("committed text makes overflow terminal");

    assert_eq!(error.kind(), Some(ModelFailureKind::ContextOverflow));
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(trace.count("context_compaction_epoch"), 0);
    assert_eq!(trace.count("assistant_delta"), 1);
  }

  #[test]
  fn reasoning_before_context_overflow_is_not_replayed() {
    let provider = Scripted::new(
      "reasoning-overflow",
      vec![vec![ProviderEvent::ReasoningDelta {
        text: "already committed".into(),
        provenance: ReasoningProvenance::Native,
      }]],
    )
    .fails_after_stream(ModelFailureKind::ContextOverflow);
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let mut trace = Recorder::default();
    let error = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_messages(vec![Message::user("old history")])
    .run_turn("new turn", &CancelToken::new(), &mut SilentProgress)
    .expect_err("committed reasoning makes overflow terminal");

    assert_eq!(error.kind(), Some(ModelFailureKind::ContextOverflow));
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(trace.count("context_compaction_epoch"), 0);
    assert_eq!(trace.count("reasoning_delta"), 1);
  }

  #[test]
  fn decoded_tool_call_before_context_overflow_is_not_replayed() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = Scripted::new(
      "tool-overflow",
      vec![tool_call("spy", serde_json::json!({"value": 1}))],
    )
    .fails_after_stream(ModelFailureKind::ContextOverflow);
    let tools = registry_with(vec![Box::new(Spy(Arc::clone(&seen)))]);
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let mut trace = Recorder::default();
    let error = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_messages(vec![Message::user("old history")])
    .run_turn("new turn", &CancelToken::new(), &mut SilentProgress)
    .expect_err("a decoded call commits the failed request");

    assert_eq!(error.kind(), Some(ModelFailureKind::ContextOverflow));
    assert_eq!(provider.requests().len(), 1);
    assert!(seen.lock().unwrap().is_empty());
    assert_eq!(trace.count("context_compaction_epoch"), 0);
    assert_eq!(trace.count("tool_requested"), 1);
    assert_eq!(trace.count("tool_failed"), 1);
  }

  #[test]
  fn decoded_tool_call_before_stream_failure_closes_store_wal() {
    let provider = Scripted::new(
      "store-tool-overflow",
      vec![tool_call("spy", serde_json::json!({"value": 1}))],
    )
    .fails_after_stream(ModelFailureKind::ContextOverflow);
    let tools = registry_with(vec![Box::new(Spy(Arc::new(Mutex::new(Vec::new()))))]);
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let temp = rupi_store::TempDir::new("runtime-unexecuted-tool-wal");
    let store = rupi_store::Store::open(temp.path(), rupi_store::WritePolicy::default()).unwrap();
    let session_id = SessionId::new();
    let model = provider.model().clone();
    let session = store
      .begin(SessionHeader {
        session_id: session_id.clone(),
        version: rupi_core::session::SESSION_SCHEMA_VERSION,
        started_at_ms: 1,
        working_dir: "/workspace".into(),
        model,
        parent_session: None,
        branched_from_event: None,
        imported_from: None,
      })
      .unwrap();
    let mut trace = StoreTrace::new(session);
    let trace_path = trace.session().trace_path().to_path_buf();
    let result = {
      let mut runtime = TurnLoop::new(
        &provider,
        &tools,
        &policy,
        &mut trace,
        session_id.clone(),
        TraceId::new(),
      )
      .with_messages(vec![Message::user("old history")]);
      runtime.run_turn("new turn", &CancelToken::new(), &mut SilentProgress)
    };
    let error = result.expect_err("the incomplete provider response remains the turn outcome");
    assert_eq!(error.kind(), Some(ModelFailureKind::ContextOverflow));

    trace
      .into_session()
      .finish()
      .expect("no-message tool failure closes the projection WAL");
    let restored = store
      .restore(&session_id)
      .expect("the finished session restores immediately");
    assert!(restored.interrupted_tools.is_empty());
    assert_eq!(restored.messages.len(), 2);
    assert_eq!(
      restored
        .messages
        .iter()
        .filter(|message| message.message.role == Role::Tool)
        .count(),
      1,
      "the known-unexecuted call has a durable failed result"
    );

    let journal = rupi_store::TraceJournal::read(&trace_path).unwrap();
    let requested = journal
      .items
      .iter()
      .find(|item| matches!(item.envelope.event, AgentEvent::ToolRequested(_)))
      .expect("the decoded call is durably requested");
    let failed = journal
      .items
      .iter()
      .find(|item| matches!(item.envelope.event, AgentEvent::ToolFailed(_)))
      .expect("the unexecuted call is durably failed");
    assert_eq!(
      failed.envelope.meta.parent_event_id,
      Some(requested.envelope.meta.event_id.clone())
    );
  }

  #[test]
  fn overflow_without_prior_history_is_terminal() {
    let provider = Scripted::new("no-history", Vec::new()).fails(
      0,
      ModelFailure::new(
        ModelFailureKind::ContextOverflow,
        FailurePhase::WaitingForResponse,
        "context window exceeded",
      ),
    );
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let mut trace = Recorder::default();
    let error = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .run_turn(
      "only current-turn content",
      &CancelToken::new(),
      &mut SilentProgress,
    )
    .expect_err("the current turn cannot be summarized away");

    assert_eq!(error.kind(), Some(ModelFailureKind::ContextOverflow));
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(trace.count("context_compaction_epoch"), 0);
  }

  #[test]
  fn overflow_preserves_external_and_tool_context_verbatim() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let overflow = ModelFailure::new(
      ModelFailureKind::ContextOverflow,
      FailurePhase::WaitingForResponse,
      "context window exceeded",
    );
    let provider = Scripted::new(
      "preserve-turn",
      vec![
        tool_call("spy", serde_json::json!({"value": 1})),
        text("done"),
      ],
    )
    .fails(1, overflow);
    let tools = registry_with(vec![Box::new(Spy(Arc::clone(&seen)))]);
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let mut trace = Recorder::default();
    let source = rupi_core::trace::ExternalContextSource {
      provider: "fixture".into(),
      resource_id: "doc-1".into(),
      provenance: "fixture/source".into(),
    };
    let external = ExternalContextItem::inline(source, "external evidence", Some("C1".into()));
    let mut runtime = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_messages(vec![Message::user("old history")]);

    runtime
      .run_turn_with_external_context(
        "new turn",
        &[external],
        &CancelToken::new(),
        &mut SilentProgress,
      )
      .expect("the reissue completes");

    assert_eq!(seen.lock().unwrap().len(), 1);
    let requests = provider.requests();
    assert_eq!(requests.len(), 3);
    let reissue = &requests[2];
    assert!(
      reissue.messages[0]
        .text()
        .contains("Summary of earlier conversation")
    );
    assert!(reissue.messages[1].text().contains("external evidence"));
    assert_eq!(reissue.messages[2].text(), "new turn");
    assert!(matches!(reissue.messages[3].role, Role::Assistant));
    assert!(matches!(reissue.messages[4].role, Role::Tool));
    assert_eq!(trace.count("context_compaction_epoch"), 1);
  }

  #[test]
  fn overflow_recovery_request_budget_resets_on_the_next_turn() {
    let provider = Scripted::new(
      "budget-reset",
      vec![text("first answer"), text("second answer")],
    )
    .fails(
      0,
      ModelFailure::new(
        ModelFailureKind::ContextOverflow,
        FailurePhase::WaitingForResponse,
        "context window exceeded",
      ),
    );
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let mut trace = Recorder::default();
    let mut runtime = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_messages(vec![Message::user("old history")]);

    let first = runtime
      .run_turn("first", &CancelToken::new(), &mut SilentProgress)
      .expect("first turn recovers");
    let second = runtime
      .run_turn("second", &CancelToken::new(), &mut SilentProgress)
      .expect("second turn starts with a fresh budget");

    assert_eq!(first.requests, 2);
    assert_eq!(second.requests, 1);
    assert_eq!(provider.requests().len(), 3);
  }

  #[test]
  fn overflow_summary_bounding_is_utf8_safe() {
    let provider = {
      let mut provider = Scripted::new("utf8-overflow", vec![text("done")]).fails(
        0,
        ModelFailure::new(
          ModelFailureKind::ContextOverflow,
          FailurePhase::WaitingForResponse,
          "context window exceeded",
        ),
      );
      provider.capabilities.context_window = 128;
      provider
    };
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let mut trace = Recorder::default();
    let mut runtime = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_messages(vec![Message::user("old history")])
    .with_summarizer(|_| "한국어 문장 日本語の文章 🙂🚀 ".repeat(200));

    runtime
      .run_turn("new turn", &CancelToken::new(), &mut SilentProgress)
      .expect("bounded UTF-8 summary can be reissued");

    let summary = provider.requests()[1].messages[0].text();
    assert!(std::str::from_utf8(summary.as_bytes()).is_ok());
    assert!(summary.len() < 128 * 4);
    assert_eq!(truncate_utf8_to_bytes("한국어🙂🚀", 1), "");
    assert_eq!(truncate_utf8_to_bytes("한국어🙂🚀", 9), "한국어");
  }

  #[test]
  fn system_prompt_participates_in_overflow_candidate_sizing() {
    let mut provider = Scripted::new("system-size", vec![text("unused")]).fails(
      0,
      ModelFailure::new(
        ModelFailureKind::ContextOverflow,
        FailurePhase::WaitingForResponse,
        "context window exceeded",
      ),
    );
    provider.capabilities.context_window = 2_000;
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let mut trace = Recorder::default();
    let error = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_system("system ".repeat(2_000))
    .with_messages(vec![Message::user("old history")])
    .run_turn("new turn", &CancelToken::new(), &mut SilentProgress)
    .expect_err("the system prompt consumes the recovery budget");

    assert_eq!(error.kind(), Some(ModelFailureKind::ContextOverflow));
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(trace.count("context_compaction_epoch"), 0);
  }

  #[test]
  fn tool_schema_participates_in_overflow_candidate_sizing() {
    let mut provider = Scripted::new("tool-size", vec![text("unused")]).fails(
      0,
      ModelFailure::new(
        ModelFailureKind::ContextOverflow,
        FailurePhase::WaitingForResponse,
        "context window exceeded",
      ),
    );
    provider.capabilities.context_window = 2_000;
    let large = "schema ".repeat(600);
    let tools = registry_with(vec![Box::new(SizedTool {
      description: large.clone(),
      schema: serde_json::json!({"type":"object","description":large}),
    })]);
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let mut trace = Recorder::default();
    let error = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_messages(vec![Message::user("old history")])
    .run_turn("new turn", &CancelToken::new(), &mut SilentProgress)
    .expect_err("tool schemas consume the recovery budget");

    assert_eq!(error.kind(), Some(ModelFailureKind::ContextOverflow));
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(trace.count("context_compaction_epoch"), 0);
  }

  #[test]
  fn compaction_sink_failure_does_not_mutate_live_history() {
    let provider = Scripted::new("compaction-failure", vec![text("unused")]).fails(
      0,
      ModelFailure::new(
        ModelFailureKind::ContextOverflow,
        FailurePhase::WaitingForResponse,
        "context window exceeded",
      ),
    );
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let mut trace = FailingCompactionSink {
      summary_event: false,
    };
    let mut runtime = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_messages(vec![Message::user("old history")]);

    let error = runtime
      .run_turn("new turn", &CancelToken::new(), &mut SilentProgress)
      .expect_err("compaction persistence failure is terminal");

    assert!(matches!(error, TurnError::Sink(message) if message.contains("compaction summary")));
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(runtime.context_epoch, 0);
    assert_eq!(
      runtime
        .messages()
        .iter()
        .map(Message::text)
        .collect::<Vec<_>>(),
      ["old history", "new turn"]
    );
  }

  #[test]
  fn assembled_request_keeps_production_shape_and_order() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let provider = Scripted::new("assembly", vec![text("ok")]);
    let tools = registry_with(vec![Box::new(Spy(Arc::clone(&seen)))]);
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let mut trace = Recorder::default();
    let mut runtime = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_system("system instructions")
    .with_thinking(ThinkingLevel::High);

    runtime
      .run_turn("first", &CancelToken::new(), &mut SilentProgress)
      .unwrap();
    let request = &provider.requests()[0];

    assert!(request.system.as_deref().is_some_and(|system| {
      system.starts_with("system instructions\n\nTools available for this request: spy.")
    }));
    assert_eq!(request.thinking, ThinkingLevel::High);
    assert_eq!(request.tools.len(), 1);
    assert_eq!(request.tools[0].name, "spy");
    assert_eq!(request.messages.len(), 1);
    assert_eq!(request.messages[0].text(), "first");
  }

  #[test]
  fn reasoning_carries_its_provenance_to_the_surface() {
    let provider = Scripted::new(
      "thinks",
      vec![vec![
        ProviderEvent::ReasoningDelta {
          text: "weighing options".into(),
          provenance: rupi_core::ReasoningProvenance::Native,
        },
        ProviderEvent::TextDelta("answer".into()),
      ]],
    );
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let mut trace = Recorder::default();
    let mut seen = ProvenanceSpy::default();
    let mut harness = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    );

    harness
      .run_turn("why", &CancelToken::new(), &mut seen)
      .unwrap();

    // The surface saw the reasoning *and* its provenance, un-collapsed.
    assert_eq!(
      seen.reasoning,
      vec![("weighing options".to_string(), "native".to_string())]
    );
    // The completed request records which kind of reasoning it streamed, so a later
    // reader can tell native output from a reconstructed rationale.
    let completed = trace.find("model_request_completed").expect("completed");
    assert_eq!(completed["reasoning_provenance"], "native");
  }

  #[test]
  fn cache_aware_token_usage_is_preserved_in_completed_request_event() {
    let usage = CompletionUsage {
      input_tokens: Some(100),
      uncached_input_tokens: Some(20),
      logical_prompt_tokens: Some(100),
      cache_read_tokens: Some(70),
      cache_write_tokens: Some(10),
      output_tokens: Some(8),
      provider_total_tokens: Some(108),
      finish_reason: Some("stop".into()),
      certainty: rupi_core::CompletionCertainty::Certain,
    };
    let provider = Scripted::new("usage", vec![text("answer")]).with_usage(usage);
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let mut trace = Recorder::default();

    TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .run_turn("answer", &CancelToken::new(), &mut SilentProgress)
    .unwrap();

    let completed = trace
      .find("model_request_completed")
      .expect("completed request");
    assert_eq!(completed["input_tokens"], 100);
    assert_eq!(completed["uncached_input_tokens"], 20);
    assert_eq!(completed["logical_prompt_tokens"], 100);
    assert_eq!(completed["cache_read_tokens"], 70);
    assert_eq!(completed["cache_write_tokens"], 10);
    assert_eq!(completed["provider_total_tokens"], 108);
  }

  #[derive(Default)]
  struct ProvenanceSpy {
    reasoning: Vec<(String, String)>,
  }

  impl TurnProgress for ProvenanceSpy {
    fn on_reasoning(&mut self, text: &str, provenance: ReasoningProvenance) {
      self
        .reasoning
        .push((text.to_string(), provenance.as_str().to_string()));
    }
  }

  #[test]
  fn reasoning_only_output_on_failure_is_committed_without_recovery() {
    let primary = Scripted::new(
      "reasoning-fails",
      vec![vec![ProviderEvent::ReasoningDelta {
        text: "already shown".into(),
        provenance: ReasoningProvenance::Native,
      }]],
    )
    .fails_after_stream(ModelFailureKind::Transport);
    let backup = Scripted::new("backup", vec![text("fallback")]);
    let tools = registry_with(Vec::new());
    let mut trace = Recorder::default();
    let mut seen = ProvenanceSpy::default();
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      primary.capabilities().context_window,
    );

    let error = TurnLoop::new(
      &primary,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_backup(&backup)
    .run_turn("reason", &CancelToken::new(), &mut seen)
    .expect_err("a mid-stream provider failure is terminal after output");

    assert_eq!(error.kind(), Some(ModelFailureKind::Transport));
    assert_eq!(
      primary.requests().len(),
      1,
      "reasoning was already committed"
    );
    assert!(
      backup.requests().is_empty(),
      "committed output forbids takeover"
    );
    assert_eq!(seen.reasoning.len(), 1, "reasoning must not be duplicated");
    assert!(trace.find("model_retry").is_none());
    assert!(trace.find("model_failover").is_none());
  }

  /// Everything a test needs to drive one loop.
  ///
  /// Built in one place so a test that asserts behavior is not also asserting
  /// wiring, and so every test sees the same policy and workspace setup.
  #[test]
  fn a_tool_call_runs_and_its_result_becomes_the_next_request() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let spy = Box::new(Spy(Arc::clone(&seen)));
    let tools = registry_with(vec![spy]);
    let provider = Scripted::new(
      "caller",
      vec![
        tool_call("spy", serde_json::json!({"question": "why"})),
        text("because"),
      ],
    );
    let mut trace = Recorder::default();

    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let report = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .run_turn("go", &CancelToken::new(), &mut SilentProgress)
    .unwrap();

    assert_eq!(report.tool_calls, 1);
    assert_eq!(report.requests, 2, "the result must go back to the model");
    assert_eq!(report.text, "because");
    assert_eq!(
      seen.lock().unwrap().as_slice(),
      &[serde_json::json!({"question": "why"})]
    );

    // The model saw the tool result as a tool message, not as prose.
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let fed = requests[1]
      .messages
      .iter()
      .find(|message| message.role == rupi_core::Role::Tool)
      .expect("tool result message");
    let ContentBlock::ToolResult(result) = &fed.content[0] else {
      panic!("expected a tool result block");
    };
    assert_eq!(result.text, "noted");
    assert!(!result.is_error);
    assert_eq!(result.state, ToolExecutionState::Succeeded);

    // Each lifecycle stage must appear exactly once: a duplicated `tool_started`
    // would make replay report a call that ran twice.
    assert_eq!(trace.count("tool_requested"), 1);
    assert_eq!(trace.count("tool_started"), 1);
    assert_eq!(trace.count("tool_completed"), 1);
    assert_eq!(
      trace.count("assistant_delta"),
      1,
      "tool progress must not masquerade as assistant output"
    );

    // Causal parenting keeps a reused provider call id from collapsing two
    // invocations in recovery: assistant completion -> request -> start -> outcome.
    let assistant_completion = trace.causal("model_request_completed")[0].0.clone();
    let requested = trace.causal("tool_requested")[0].clone();
    let started = trace.causal("tool_started")[0].clone();
    let completed = trace.causal("tool_completed")[0].clone();
    assert_eq!(requested.1, Some(assistant_completion));
    assert_eq!(started.1, Some(requested.0));
    assert_eq!(completed.1, Some(started.0));

    // And in order, which is what a replay needs to reconstruct the call honestly.
    let kinds = trace.kinds();
    let at = |wanted: &str| kinds.iter().position(|k| k == wanted).unwrap();
    assert!(
      at("tool_requested") < at("tool_started") && at("tool_started") < at("tool_completed"),
      "{kinds:?}"
    );
  }

  #[test]
  fn rejected_tool_calls_are_returned_to_the_model_without_execution() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tools = registry_with(vec![Box::new(Spy(Arc::clone(&seen)))]);
    let call_id = rupi_core::ToolCallId::new();
    let provider = Scripted::new(
      "malformed-call",
      vec![
        vec![ProviderEvent::ToolCallRejected {
          id: call_id,
          name: "spy".into(),
          reason: "tool arguments are not valid JSON: unexpected end".into(),
        }],
        text("I corrected the request."),
      ],
    );
    let mut trace = Recorder::default();
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );

    let report = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .run_turn("inspect", &CancelToken::new(), &mut SilentProgress)
    .unwrap();

    assert_eq!(report.status, TurnStatus::Completed);
    assert_eq!(report.requests, 2);
    assert_eq!(report.text, "I corrected the request.");
    assert!(seen.lock().unwrap().is_empty(), "the tool must never run");
    assert_eq!(trace.count("tool_started"), 0);
    assert_eq!(trace.count("tool_failed"), 1);
    let requests = provider.requests();
    let fed = requests[1]
      .messages
      .iter()
      .find(|message| message.role == Role::Tool)
      .expect("the failed result is returned to the same model");
    let ContentBlock::ToolResult(result) = &fed.content[0] else {
      panic!("expected a tool result block");
    };
    assert_eq!(result.state, ToolExecutionState::Failed);
    assert!(result.is_error);
    assert!(result.text.contains("not run"));
    assert!(result.text.contains("corrected tool call"));
  }

  #[test]
  fn progress_boundary_narrows_the_next_request_to_configured_tools() {
    let read_seen = Arc::new(Mutex::new(Vec::new()));
    let write_seen = Arc::new(Mutex::new(Vec::new()));
    let tools = registry_with(vec![
      Box::new(Spy(Arc::clone(&read_seen))),
      Box::new(MutatingSpy {
        seen: Arc::clone(&write_seen),
        outcome: ToolOutcome::succeeded("mutated"),
      }),
    ]);
    let provider = Scripted::new(
      "progress-boundary",
      vec![
        tool_call("spy", serde_json::json!({})),
        tool_call("write_probe", serde_json::json!({"path": "app.py"})),
        tool_call("spy", serde_json::json!({"after": "write"})),
        text("done"),
      ],
    );
    let mut trace = Recorder::default();
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );

    let report = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_progress_boundary(Some(1), vec!["write_probe".into()])
    .run_turn(
      "build the project",
      &CancelToken::new(),
      &mut SilentProgress,
    )
    .expect("progress boundary should preserve a valid tool loop");

    assert_eq!(report.status, TurnStatus::Completed);
    assert_eq!(report.text, "done");
    assert_eq!(read_seen.lock().unwrap().len(), 2);
    assert_eq!(write_seen.lock().unwrap().len(), 1);
    let requests = provider.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(
      requests[1]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>(),
      vec!["write_probe"]
    );
    assert!(
      requests[1]
        .system
        .as_deref()
        .unwrap()
        .contains("Tools available for this request: write_probe.")
    );
    assert_eq!(
      requests[2].tools.len(),
      2,
      "a successful progress tool ends the one-shot boundary"
    );
    assert!(
      requests[2]
        .system
        .as_deref()
        .unwrap()
        .contains("Tools available for this request: spy, write_probe.")
    );
    assert!(
      requests[1]
        .messages
        .iter()
        .any(|message| message.text().contains("Runtime progress boundary")),
      "the boundary must be visible to the model"
    );
    let diagnostics = trace.0.lock().unwrap();
    assert!(diagnostics.iter().any(|(_, kind, payload)| {
      kind == "diagnostic"
        && payload["message"]
          .as_str()
          .unwrap_or_default()
          .contains("progress boundary active")
    }));
  }

  #[test]
  fn failed_progress_attempt_keeps_the_boundary_narrowed() {
    let read_seen = Arc::new(Mutex::new(Vec::new()));
    let write_seen = Arc::new(Mutex::new(Vec::new()));
    let tools = registry_with(vec![
      Box::new(Spy(Arc::clone(&read_seen))),
      Box::new(MutatingSpy {
        seen: Arc::clone(&write_seen),
        outcome: ToolOutcome::failed("rejected"),
      }),
    ]);
    let provider = Scripted::new(
      "failed-progress-boundary",
      vec![
        tool_call("spy", serde_json::json!({})),
        tool_call("write_probe", serde_json::json!({"path": "app.py"})),
        tool_call("write_probe", serde_json::json!({"path": "app.py"})),
        text("done"),
      ],
    );
    let mut trace = Recorder::default();
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );

    let report = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_progress_boundary(Some(1), vec!["write_probe".into()])
    .run_turn(
      "build the project",
      &CancelToken::new(),
      &mut SilentProgress,
    )
    .expect("a failed progress attempt should remain recoverable");

    assert_eq!(report.status, TurnStatus::Completed);
    assert_eq!(report.text, "done");
    assert_eq!(read_seen.lock().unwrap().len(), 1);
    assert_eq!(write_seen.lock().unwrap().len(), 2);
    let requests = provider.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(
      requests[1]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>(),
      vec!["write_probe"]
    );
    assert_eq!(
      requests[2]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<Vec<_>>(),
      vec!["write_probe"],
      "failed progress must not restore the unrestricted tool set"
    );
  }

  #[test]
  fn reduced_builtin_output_is_archived_and_redacted() {
    let temp = rupi_store::TempDir::new("runtime-reduced-read");
    let secret = "recovery-secret-91f7";
    let body: String = (1..=20_000)
      .map(|line| format!("{secret} line {line}\n"))
      .collect();
    std::fs::write(temp.child("large.txt"), body).unwrap();

    let mut write_policy = rupi_store::WritePolicy::default();
    write_policy.redaction.scan_environment = false;
    write_policy.redaction.literals = vec![secret.into()];
    let store = rupi_store::Store::open(temp.path(), write_policy).unwrap();
    let session_id = SessionId::new();
    let model = ModelRef::new("test", "read");
    let session = store
      .begin(SessionHeader {
        session_id: session_id.clone(),
        version: rupi_core::session::SESSION_SCHEMA_VERSION,
        started_at_ms: 1,
        working_dir: temp.path().display().to_string(),
        model: model.clone(),
        parent_session: None,
        branched_from_event: None,
        imported_from: None,
      })
      .unwrap();
    let mut trace = StoreTrace::new(session);
    let tools = ToolRegistry::new(
      Workspace::new(temp.path())
        .unwrap()
        .with_read_outside(false),
    )
    .with_policy(&ToolPolicy {
      max_output_bytes: 1_024,
      ..ToolPolicy::default()
    })
    .with_builtins();
    let provider = Scripted::new(
      "read",
      vec![
        tool_call("read", serde_json::json!({"path": "large.txt"})),
        text("done"),
      ],
    );
    let context = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );

    let report = TurnLoop::new(
      &provider,
      &tools,
      &context,
      &mut trace,
      session_id,
      TraceId::new(),
    )
    .run_turn(
      "read the large file",
      &CancelToken::new(),
      &mut SilentProgress,
    )
    .unwrap();
    assert_eq!(report.text, "done");

    let trace_path = trace.session().trace_path().to_path_buf();
    let session_path = trace.session().path().to_path_buf();
    let entries = rupi_store::TraceJournal::read(&trace_path).unwrap().items;
    let reduced = entries
      .iter()
      .find_map(|entry| match &entry.envelope.event {
        AgentEvent::ContextReduced(reduced) => Some(reduced.clone()),
        _ => None,
      })
      .expect("a reduced built-in result must have a recovery event");
    assert!(reduced.original_bytes > reduced.visible_bytes);
    // A store-backed reduction is the recoverable case, and this asserts it as such:
    // the pointer and the reference are both present.
    let reference = reduced
      .recovery_ref
      .expect("a reduction with a blob store is recoverable");
    assert!(reference.contains("blobs/"), "{reference}");
    let blob = reduced.blob.expect("the reference travels with the blob");
    let blob_path = trace.session().blobs().path_for(&blob);
    let blob_text = std::fs::read_to_string(blob_path).unwrap();
    assert!(blob_text.contains("[redacted:field]"), "{blob_text}");
    assert!(
      !blob_text.contains(secret),
      "secret leaked into recovery blob"
    );

    let trace_text = std::fs::read_to_string(trace_path).unwrap();
    let session_text = std::fs::read_to_string(session_path).unwrap();
    assert!(!trace_text.contains(secret), "secret leaked into trace");
    assert!(!session_text.contains(secret), "secret leaked into session");
  }

  struct FailingToolStart;

  impl Trace for FailingToolStart {
    fn emit(&mut self, envelope: &mut EventEnvelope) -> Result<(), SinkError> {
      if matches!(envelope.event, AgentEvent::ToolStarted(_)) {
        return Err(SinkError("cannot persist tool start".into()));
      }
      Ok(())
    }
  }

  #[test]
  fn tool_does_not_run_when_its_start_cannot_be_persisted() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tools = registry_with(vec![Box::new(Spy(Arc::clone(&seen)))]);
    let provider = Scripted::new(
      "tool-start-failure",
      vec![tool_call("spy", serde_json::json!({}))],
    );
    let mut trace = FailingToolStart;
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );

    let error = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .run_turn("go", &CancelToken::new(), &mut SilentProgress)
    .unwrap_err();

    assert!(matches!(error, TurnError::Sink(message) if message.contains("tool start")));
    assert!(seen.lock().unwrap().is_empty());
  }

  #[test]
  fn workspace_path_refusals_do_not_emit_tool_started() {
    let workspace_dir = rupi_store::TempDir::new("runtime-workspace");
    let outside = rupi_store::TempDir::new("runtime-outside");
    let outside_file = outside.child("outside.txt");
    std::fs::write(&outside_file, "outside\n").unwrap();
    let outside_file = outside_file.display().to_string();
    let outside_dir = outside.path().display().to_string();
    let call = |id: &str, name: &str, arguments: serde_json::Value| {
      ProviderEvent::ToolCall(ToolCallBlock {
        id: rupi_core::ToolCallId::from_string(id),
        name: name.into(),
        arguments,
      })
    };
    let calls = vec![
      call(
        "read-outside",
        "read",
        serde_json::json!({"path": outside_file.clone()}),
      ),
      call(
        "write-outside",
        "write",
        serde_json::json!({"path": format!("{outside_dir}/new.txt"), "contents": "no"}),
      ),
      call(
        "edit-outside",
        "edit",
        serde_json::json!({"path": outside_file.clone(), "find": "outside", "replace": "changed"}),
      ),
      call(
        "grep-outside",
        "grep",
        serde_json::json!({"pattern": "outside", "path": outside_dir}),
      ),
      call(
        "exec-outside",
        "exec",
        serde_json::json!({"command": "pwd", "cwd": outside_dir}),
      ),
    ];
    let provider = Scripted::new("path-policy", vec![calls, text("done")]);
    let tools = ToolRegistry::new(
      Workspace::new(workspace_dir.path())
        .unwrap()
        .with_read_outside(false),
    )
    .with_policy(&ToolPolicy {
      auto_approve_mutating: true,
      ..ToolPolicy::default()
    })
    .with_builtins();
    let mut trace = Recorder::default();
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );

    let report = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .run_turn(
      "stay in the workspace",
      &CancelToken::new(),
      &mut SilentProgress,
    )
    .unwrap();

    assert_eq!(report.tool_calls, 5);
    assert_eq!(report.text, "done");
    assert_eq!(trace.count("tool_requested"), 5);
    assert_eq!(trace.count("tool_failed"), 5);
    assert_eq!(
      trace.count("tool_started"),
      0,
      "refusals never began execution"
    );
    assert_eq!(std::fs::read_to_string(&outside_file).unwrap(), "outside\n");
    assert!(!outside.child("new.txt").exists());
  }

  #[test]
  fn unknown_and_invalid_calls_never_claim_they_started() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tools = registry_with(vec![Box::new(Spy(Arc::clone(&seen)))]);
    let provider = Scripted::new(
      "bad-calls",
      vec![
        vec![
          ProviderEvent::ToolCall(ToolCallBlock {
            id: rupi_core::ToolCallId::from_string("unknown-call"),
            name: "missing".into(),
            arguments: serde_json::json!({}),
          }),
          ProviderEvent::ToolCall(ToolCallBlock {
            id: rupi_core::ToolCallId::from_string("invalid-call"),
            name: "spy".into(),
            arguments: serde_json::json!("not an object"),
          }),
        ],
        text("done"),
      ],
    );
    let mut trace = Recorder::default();
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );

    TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .run_turn("go", &CancelToken::new(), &mut SilentProgress)
    .unwrap();

    assert!(seen.lock().unwrap().is_empty());
    assert_eq!(trace.count("tool_requested"), 2);
    assert_eq!(trace.count("tool_failed"), 2);
    assert_eq!(trace.count("tool_started"), 0);
  }

  #[test]
  fn a_transient_failure_retries_the_same_model_before_any_takeover() {
    // Only one round: a failed request consumes none, so the retry re-serves the
    // first answer rather than skipping to a second one.
    let provider = Scripted::new("flaky", vec![text("second time")]).fails(
      0,
      ModelFailure::new(
        ModelFailureKind::Transport,
        FailurePhase::WaitingForResponse,
        "reset",
      ),
    );
    let tools = registry_with(Vec::new());
    let mut trace = Recorder::default();

    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let report = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .run_turn("hi", &CancelToken::new(), &mut SilentProgress)
    .unwrap();

    assert_eq!(report.text, "second time");
    assert_eq!(report.requests, 2);
    assert!(!report.epoch > 0, "a retry is not a takeover");
    let retry = trace.find("model_retry").expect("retry recorded");
    assert_eq!(retry["kind"], "transport");
    // The retry is the second request against a model that already had one.
    assert_eq!(retry["attempt"], 2, "{retry}");
    // Same model both times: the first failure was never given away.
    assert_eq!(provider.levels().len(), 2);
    assert_eq!(report.epoch, 0, "a retry stays in the first epoch");
    assert!(trace.find("model_failover").is_none());
  }

  #[test]
  fn a_still_failing_model_lets_the_backup_answer() {
    let primary = Scripted::new("primary", vec![text("primary answer")])
      .always_fails(ModelFailureKind::ProviderUnavailable);
    let backup = Scripted::new("backup", vec![text("from backup")]);
    let tools = registry_with(Vec::new());
    let mut trace = Recorder::default();
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      primary.capabilities().context_window,
    );
    let report = TurnLoop::new(
      &primary,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_backup(&backup)
    .run_turn("hi", &CancelToken::new(), &mut SilentProgress)
    .unwrap();

    assert_eq!(report.text, "from backup");
    assert_eq!(report.epoch, 1, "the answer belongs to the second epoch");
    let failover = trace.find("model_failover").expect("failover recorded");
    assert_eq!(failover["from"], "test/primary");
    assert_eq!(failover["to"], "test/backup");
    assert_eq!(failover["kind"], "provider_unavailable");
    // The epoch transition is recorded *after* the failover that caused it, so replay
    // rebuilds the epoch list in the order things actually happened.
    let kinds = trace.kinds();
    let failover_at = kinds.iter().rposition(|k| k == "model_failover").unwrap();
    let epoch_at = kinds[failover_at..]
      .iter()
      .position(|k| k == "model_epoch_started")
      .expect("the failover must be followed by the epoch it created");
    assert_eq!(
      epoch_at, 1,
      "the epoch event must immediately follow: {kinds:?}"
    );
    assert_eq!(
      kinds.iter().filter(|k| *k == "model_epoch_started").count(),
      2
    );
  }

  #[test]
  fn a_policy_override_keeps_the_attached_backup() {
    // The builders are separate because the caller owns the provider and the
    // operator tunes the policy. An override that quietly dropped the backup would
    // leave a configured, attached, and permanently unused backup.
    let primary = Scripted::new("primary", vec![text("primary answer")])
      .always_fails(ModelFailureKind::ProviderUnavailable);
    let backup = Scripted::new("backup", vec![text("from backup")]);
    let tools = registry_with(Vec::new());
    let mut trace = Recorder::default();
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      primary.capabilities().context_window,
    );
    let report = TurnLoop::new(
      &primary,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_backup(&backup)
    .with_failover(FailoverPolicy::default().with_max_attempts(3))
    .run_turn("hi", &CancelToken::new(), &mut SilentProgress)
    .expect("the backup answers");

    assert_eq!(report.text, "from backup");
    assert_eq!(report.epoch, 1);
    // The override took effect: three attempts against the primary before yielding.
    assert_eq!(primary.levels().len(), 3);
  }

  #[test]
  fn a_policy_override_preserves_the_primary_capability_gate() {
    // Changing retry tuning must not replace the session's required capabilities
    // with FailoverPolicy's text-only default. The backup lacks tools and must be
    // refused even though the replacement policy is otherwise valid.
    let primary =
      Scripted::new("primary", Vec::new()).always_fails(ModelFailureKind::ProviderUnavailable);
    let mut backup = Scripted::new("backup", vec![text("must not run")]);
    backup.capabilities.tools = false;
    let tools = registry_with(Vec::new());
    let mut trace = Recorder::default();
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      primary.capabilities().context_window,
    );
    let error = TurnLoop::new(
      &primary,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_backup(&backup)
    .with_failover(FailoverPolicy::default().with_max_attempts(1))
    .run_turn("hi", &CancelToken::new(), &mut SilentProgress)
    .expect_err("the text-only backup cannot serve a tool-capable session");

    assert!(matches!(error, TurnError::Unavailable(_)), "{error:?}");
    assert_eq!(backup.levels().len(), 0, "the capability gate refuses it");
    assert!(
      trace
        .diagnostics()
        .iter()
        .any(|message| message.contains("tool calling")),
      "the refusal names the preserved requirement: {:?}",
      trace.diagnostics()
    );
  }

  #[test]
  fn a_detached_backup_is_never_asked() {
    let primary = Scripted::new("primary", Vec::new()).always_fails(ModelFailureKind::Transport);
    let backup = Scripted::new("backup", vec![text("unused")]);
    let tools = registry_with(Vec::new());
    let mut trace = Recorder::default();
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      primary.capabilities().context_window,
    );
    let error = TurnLoop::new(
      &primary,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_backup(&backup)
    .without_backup()
    .run_turn("hi", &CancelToken::new(), &mut SilentProgress)
    .expect_err("a down primary with no backup is fatal");

    assert!(matches!(error, TurnError::Unavailable(_)), "{error:?}");
    assert_eq!(backup.levels().len(), 0, "detached means never asked");
    assert_eq!(trace.count("model_failover"), 0);
  }

  #[test]
  fn a_backup_that_fails_does_not_take_over_from_itself() {
    // Both models are down. The policy still names the backup, so without a guard
    // the loop would open a second epoch for the model already serving, retry it,
    // and keep converting one failure into several recorded transitions.
    let primary =
      Scripted::new("primary", Vec::new()).always_fails(ModelFailureKind::ProviderUnavailable);
    let backup =
      Scripted::new("backup", Vec::new()).always_fails(ModelFailureKind::ProviderUnavailable);
    let tools = registry_with(Vec::new());
    let mut trace = Recorder::default();
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      primary.capabilities().context_window,
    );
    let error = TurnLoop::new(
      &primary,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_backup(&backup)
    .run_turn("hi", &CancelToken::new(), &mut SilentProgress)
    .expect_err("nothing can serve this turn");

    assert!(matches!(error, TurnError::Unavailable(_)), "{error:?}");
    assert_eq!(
      trace.count("model_failover"),
      1,
      "one real takeover, no self-handover"
    );
    assert_eq!(
      trace.count("model_epoch_started"),
      2,
      "the epoch list records what actually changed: {kinds:?}",
      kinds = trace.kinds()
    );
    assert!(
      trace
        .diagnostics()
        .iter()
        .any(|message| message.contains("refused: it is the active model")),
      "the refusal must be stated, not only implied: {:?}",
      trace.diagnostics()
    );
    // Two attempts each, then stop: the guard also ends the request-budget bleed.
    assert_eq!(primary.levels().len() + backup.levels().len(), 4);
  }

  #[test]
  fn a_backup_without_tool_calling_is_refused_by_name_and_never_asked() {
    // The capability gate end to end, inside the loop: the session ran on a model
    // that calls tools, the configured backup cannot, and so the backup is not used.
    // What is new is that the refusal is *said*. Silent abstention looks identical to
    // "no backup was configured", and the operator has no way to learn that the
    // backup they set up was never a candidate for this work.
    let primary =
      Scripted::new("primary", Vec::new()).always_fails(ModelFailureKind::ProviderUnavailable);
    let mut backup = Scripted::new("backup", vec![text("unused")]);
    backup.capabilities.tools = false;
    let tools = registry_with(Vec::new());
    let mut trace = Recorder::default();
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      primary.capabilities().context_window,
    );
    let error = TurnLoop::new(
      &primary,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_backup(&backup)
    .run_turn("hi", &CancelToken::new(), &mut SilentProgress)
    .expect_err("a backup that cannot do the work is not a rescue");

    assert!(matches!(error, TurnError::Unavailable(_)), "{error:?}");
    assert_eq!(
      backup.levels().len(),
      0,
      "a refused backup is never addressed, so no cost is paid for it"
    );
    assert_eq!(
      trace.count("model_failover"),
      0,
      "a refusal is not a takeover, and must not be recorded as one"
    );
    assert!(
      trace
        .diagnostics()
        .iter()
        .any(|message| message.contains("test/backup") && message.contains("tool calling")),
      "the refusal names the backup and the missing capability: {:?}",
      trace.diagnostics()
    );
  }

  #[test]
  fn a_narrower_backup_is_not_credited_with_a_shortening_it_did_not_do() {
    // One turn is in flight, so there is no older history to drop. The takeover is
    // still correct and the window gap is still reported; what may not be reported
    // is a reduction that never happened.
    let primary =
      Scripted::new("primary", Vec::new()).always_fails(ModelFailureKind::ProviderUnavailable);
    let mut backup = Scripted::new("backup", vec![text("from a small window")]);
    backup.capabilities.context_window = 2_048;
    let tools = registry_with(Vec::new());
    let mut trace = Recorder::default();
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      primary.capabilities().context_window,
    );
    let report = TurnLoop::new(
      &primary,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_backup(&backup)
    .run_turn("hi", &CancelToken::new(), &mut SilentProgress)
    .expect("a narrower backup may still take over");

    assert_eq!(report.text, "from a small window");
    let failover = trace.find("model_failover").expect("takeover recorded");
    assert_eq!(
      failover["compacted"], false,
      "nothing was dropped, so nothing may claim it was: {failover}"
    );
    assert!(
      failover["gaps"].to_string().contains("context_window"),
      "the cost of the switch is still recorded: {failover}"
    );
    assert_eq!(
      trace.count("context_reduced"),
      0,
      "{kinds:?}",
      kinds = trace.kinds()
    );
  }

  #[test]
  fn a_narrower_backup_refuses_an_unrecoverable_tool_boundary() {
    let primary =
      Scripted::new("primary", Vec::new()).always_fails(ModelFailureKind::ProviderUnavailable);
    let mut backup = Scripted::new("backup", vec![text("must not run")]);
    backup.capabilities.context_window = 1_100;
    let mut assistant_with_open_call = Message::assistant("planning");
    assistant_with_open_call
      .content
      .push(ContentBlock::ToolCall(ToolCallBlock {
        id: rupi_core::ToolCallId::new(),
        name: "write".into(),
        arguments: serde_json::json!({"path":"state"}),
      }));
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      primary.capabilities().context_window,
    );
    let mut trace = Recorder::default();
    let mut runtime = TurnLoop::new(
      &primary,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_backup(&backup)
    .with_messages(vec![
      Message::user("a".repeat(20_000)),
      assistant_with_open_call,
      Message::user("current turn"),
    ]);
    let mut turn_start = 2;
    let error = runtime
      .rebudget(&backup, TurnId::new(), &mut turn_start)
      .expect_err("an incomplete tool unit cannot be crossed to fit the backup");

    assert!(
      matches!(error, TurnError::Sink(ref message) if message.contains("cannot safely rebudget")),
      "unexpected error: {error:?}"
    );
    assert!(
      backup.requests().is_empty(),
      "rebudget must not contact the backup"
    );
  }

  #[test]
  fn backup_rebudget_accounts_for_system_prompt_and_exposed_tool_schemas() {
    let primary =
      Scripted::new("primary", Vec::new()).always_fails(ModelFailureKind::ProviderUnavailable);
    let mut backup = Scripted::new("backup", vec![text("must not run")]);
    backup.capabilities.context_window = 1_100;
    let tools = registry_with(vec![Box::new(SizedTool {
      description: "description ".repeat(200),
      schema: serde_json::json!({"type":"object","description":"schema ".repeat(200)}),
    })]);
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      primary.capabilities().context_window,
    );
    let mut trace = Recorder::default();
    let mut runtime = TurnLoop::new(
      &primary,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_backup(&backup)
    .with_system("system ".repeat(200))
    .with_messages(vec![Message::user("small current request")]);
    let mut turn_start = 0;

    let error = runtime
      .rebudget(&backup, TurnId::new(), &mut turn_start)
      .expect_err("fixed request overhead does not fit the backup target");
    assert!(
      matches!(error, TurnError::Sink(ref message) if message.contains("complete backup request")),
      "unexpected error: {error:?}"
    );
    assert!(backup.requests().is_empty(), "the backup is not contacted");
    drop(runtime);
  }

  #[test]
  fn a_narrower_backup_drops_older_turns_before_it_takes_over() {
    // The other side of the same line: with real history in flight, takeover into a
    // small window shortens it, says so, and records what was given up.
    let primary =
      Scripted::new("primary", Vec::new()).always_fails(ModelFailureKind::ProviderUnavailable);
    let mut backup = Scripted::new("backup", vec![text("from a small window")]);
    // The backup keeps ten percent headroom, so it can hold the newest turn and
    // little else.
    backup.capabilities.context_window = 1_100;
    let history = vec![
      Message::user("a".repeat(20_000)),
      Message::assistant("b".repeat(20_000)),
    ];
    let tools = registry_with(Vec::new());
    let mut trace = Recorder::default();
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      primary.capabilities().context_window,
    );
    let report = TurnLoop::new(
      &primary,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_backup(&backup)
    .with_messages(history)
    .run_turn("continue", &CancelToken::new(), &mut SilentProgress)
    .expect("a narrower backup may still take over");

    assert_eq!(report.text, "from a small window");
    let failover = trace.find("model_failover").expect("takeover recorded");
    assert_eq!(
      failover["compacted"], true,
      "history really was shortened: {failover}"
    );
    let reduced = trace
      .find("context_reduced")
      .expect("a dropped history is recorded, not silently lost");
    assert!(
      reduced["original_bytes"].as_u64().unwrap_or_default()
        > reduced["visible_bytes"].as_u64().unwrap_or_default(),
      "the record shows what was given up: {reduced}"
    );
    // And the backup is asked with what it can actually hold.
    let served = backup
      .requests()
      .into_iter()
      .last()
      .expect("the backup was asked");
    assert_eq!(
      served.messages.len(),
      1,
      "only the turn in flight survives a window that small"
    );
  }

  #[test]
  fn a_bad_answer_is_not_an_outage() {
    // The model answered and the answer was unusable. That is a quality failure:
    // no retry, no takeover, and the failure reaches the caller.
    let provider = Scripted::new("wrong", Vec::new()).fails(
      0,
      ModelFailure::new(
        ModelFailureKind::Semantic,
        FailurePhase::Streaming,
        "not usable",
      ),
    );
    let backup = Scripted::new("backup", vec![text("nope")]);
    let tools = registry_with(Vec::new());
    let mut trace = Recorder::default();
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let error = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_backup(&backup)
    .run_turn("hi", &CancelToken::new(), &mut SilentProgress)
    .expect_err("a semantic failure is terminal");

    assert_eq!(
      error.kind().unwrap_or_else(|| panic!("{error:?}")),
      ModelFailureKind::Semantic
    );
    assert!(
      trace.find("model_failover").is_none(),
      "quality never moves the model"
    );
    assert!(trace.find("model_retry").is_none());
    // The turn still ends, so the session can say what happened to it.
    assert!(trace.kinds().iter().any(|k| k == "turn_completed"));
  }

  #[test]
  fn request_budget_covers_recovery_attempts() {
    let provider = Scripted::new("timeout", Vec::new()).always_fails(ModelFailureKind::Timeout);
    let tools = registry_with(Vec::new());
    let mut trace = Recorder::default();
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );

    let error = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_max_requests(1)
    .run_turn("bounded request", &CancelToken::new(), &mut SilentProgress)
    .expect_err("the one-request budget must stop before retry");

    assert_eq!(error.kind(), Some(ModelFailureKind::Timeout));
    assert_eq!(trace.count("model_request_started"), 1);
    assert_eq!(trace.count("model_request_completed"), 1);
    assert_eq!(trace.count("model_retry"), 0);
  }

  #[test]
  fn an_unfinished_stream_is_not_reported_as_an_answer() {
    // The provider returned Ok with an uncertain boundary: the transport ended, the
    // model did not. Accepting that as a finished turn is how a harness silently
    // truncates answers.
    let provider = Scripted::uncertain("truncated", "half an an");
    let tools = registry_with(Vec::new());
    let mut trace = Recorder::default();

    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let error = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .run_turn("story", &CancelToken::new(), &mut SilentProgress)
    .expect_err("an unfinished response is not a completed turn");

    assert_eq!(
      error.kind().unwrap_or_else(|| panic!("{error:?}")),
      ModelFailureKind::Semantic,
      "the model answered and the answer was not usable"
    );
    assert!(
      trace.find("model_retry").is_none(),
      "a half answer must not replay"
    );
    let completed = trace.find("model_request_completed").expect("completed");
    assert_eq!(
      completed["finish_reason"],
      serde_json::Value::Null,
      "no finish reason was ever observed"
    );
  }

  #[test]
  fn output_limited_completion_is_failed_and_keeps_provider_evidence() {
    let provider = Scripted::new("limited", vec![text("partial")]).finishes_with("length");
    let tools = registry_with(Vec::new());
    let mut trace = Recorder::default();
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );

    let error = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .run_turn(
      "finish the bounded task",
      &CancelToken::new(),
      &mut SilentProgress,
    )
    .expect_err("an output-limited answer is not a completed turn");

    assert_eq!(error.kind(), Some(ModelFailureKind::Semantic));
    let TurnError::Unavailable(failure) = &error else {
      panic!("expected a provider failure, got {error:?}");
    };
    assert!(failure.message.contains("output limit"));
    assert_eq!(trace.count("model_retry"), 0);
    let completed = trace
      .all("model_request_completed")
      .into_iter()
      .next()
      .expect("failed request is closed in the trace");
    assert_eq!(completed["finish_reason"], "length");
    assert_eq!(completed["output_tokens"], 4);
  }

  #[test]
  fn decoded_call_on_transport_failure_is_closed_without_execution() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tools = registry_with(vec![Box::new(Spy(Arc::clone(&seen)))]);
    let provider = Scripted::new(
      "broken-after-call",
      vec![tool_call("spy", serde_json::json!({"value": 1}))],
    )
    .fails_after_stream(ModelFailureKind::Transport);
    let mut trace = Recorder::default();
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );

    let error = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .run_turn("go", &CancelToken::new(), &mut SilentProgress)
    .unwrap_err();

    assert_eq!(error.kind(), Some(ModelFailureKind::Transport));
    assert!(seen.lock().unwrap().is_empty());
    assert_eq!(trace.count("tool_requested"), 1);
    assert_eq!(trace.count("tool_failed"), 1);
    assert_eq!(trace.count("tool_started"), 0);
  }

  #[test]
  fn decoded_mutating_call_on_uncertain_completion_is_not_executed() {
    let temp = rupi_store::TempDir::new("runtime-uncertain-write");
    let target = temp.child("must-not-exist.txt");
    let tools = ToolRegistry::new(Workspace::new(temp.path()).unwrap())
      .with_policy(&ToolPolicy {
        auto_approve_mutating: true,
        ..ToolPolicy::default()
      })
      .with_builtins();
    let provider = {
      let mut provider = Scripted::new(
        "uncertain-write",
        vec![tool_call(
          "write",
          serde_json::json!({"path": "must-not-exist.txt", "contents": "unsafe"}),
        )],
      );
      provider.unfinished = true;
      provider
    };
    let mut trace = Recorder::default();
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );

    let error = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .run_turn("write it", &CancelToken::new(), &mut SilentProgress)
    .unwrap_err();

    assert_eq!(error.kind(), Some(ModelFailureKind::Protocol));
    assert!(
      !target.exists(),
      "an uncertain decoded call must not mutate"
    );
    assert_eq!(trace.count("tool_requested"), 1);
    assert_eq!(trace.count("tool_failed"), 1);
    assert_eq!(trace.count("tool_started"), 0);
  }

  #[test]
  fn decoded_call_on_uncertain_completion_is_closed_without_execution() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tools = registry_with(vec![Box::new(Spy(Arc::clone(&seen)))]);
    let mut provider = Scripted::new(
      "uncertain-call",
      vec![tool_call("spy", serde_json::json!({"value": 1}))],
    );
    provider.unfinished = true;
    let mut trace = Recorder::default();
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );

    let error = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .run_turn("go", &CancelToken::new(), &mut SilentProgress)
    .unwrap_err();

    assert_eq!(error.kind(), Some(ModelFailureKind::Protocol));
    assert!(seen.lock().unwrap().is_empty());
    assert_eq!(trace.count("tool_requested"), 1);
    assert_eq!(trace.count("tool_failed"), 1);
    assert_eq!(trace.count("tool_started"), 0);
  }

  #[test]
  fn cancellation_settles_the_entire_assistant_tool_batch_before_resume() {
    let cancel = CancelToken::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tools = registry_with(vec![
      Box::new(CancelFirstCall),
      Box::new(Spy(Arc::clone(&seen))),
    ]);
    let mut batch = Vec::new();
    batch.extend(tool_call("cancel_first", serde_json::json!({})));
    batch.extend(tool_call("spy", serde_json::json!({"item": 2})));
    batch.extend(tool_call("spy", serde_json::json!({"item": 3})));
    let provider = Scripted::new("cancelled-batch", vec![batch, text("resumed")]);
    let mut trace = Recorder::default();
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let mut runtime = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    );

    let interrupted = runtime
      .run_turn("start", &cancel, &mut SilentProgress)
      .expect("cancellation is a durable turn result");
    assert_eq!(interrupted.status, TurnStatus::Cancelled);
    assert_eq!(seen.lock().unwrap().len(), 0, "the batch tail never runs");

    let resumed = runtime
      .run_turn("continue", &CancelToken::new(), &mut SilentProgress)
      .expect("a later turn receives protocol-valid history");
    assert_eq!(resumed.status, TurnStatus::Completed);
    let request = provider.requests().pop().expect("resume request exists");
    let calls: BTreeSet<_> = request
      .messages
      .iter()
      .filter(|message| message.role == Role::Assistant)
      .flat_map(Message::tool_calls)
      .map(|call| call.id.to_string())
      .collect();
    let results: Vec<_> = request
      .messages
      .iter()
      .filter(|message| message.role == Role::Tool)
      .flat_map(|message| &message.content)
      .filter_map(|block| match block {
        ContentBlock::ToolResult(result) => Some(result),
        _ => None,
      })
      .collect();
    let result_ids: BTreeSet<_> = results.iter().map(|result| result.id.to_string()).collect();
    assert_eq!(
      calls, result_ids,
      "every committed call has one visible result"
    );
    assert_eq!(results.len(), 3);
    assert_eq!(
      results
        .iter()
        .filter(|result| result.state == ToolExecutionState::Failed)
        .count(),
      2
    );
    drop(runtime);
    assert_eq!(trace.count("tool_completed"), 1);
    assert_eq!(trace.count("tool_failed"), 2);
  }

  #[test]
  fn a_cancelled_turn_reports_cancelled_and_runs_no_tools() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let spy = Box::new(Spy(Arc::clone(&seen)));
    let tools = registry_with(vec![spy]);
    let provider = Scripted::new("caller", vec![tool_call("spy", serde_json::json!({}))]);
    let mut trace = Recorder::default();
    let cancel = CancelToken::new();
    cancel.cancel();

    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let report = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .run_turn("go", &cancel, &mut SilentProgress)
    .unwrap();

    assert_eq!(report.status, TurnStatus::Cancelled);
    assert!(seen.lock().unwrap().is_empty(), "nothing runs after cancel");
    assert_eq!(
      trace
        .kinds()
        .iter()
        .filter(|k| *k == "tool_started")
        .count(),
      0,
      "a cancelled turn must not start tools"
    );
    assert!(trace.kinds().iter().any(|k| k == "turn_completed"));
  }

  #[test]
  fn a_model_that_never_stops_asking_costs_one_visible_failure() {
    let rounds = (0..8)
      .map(|_| tool_call("noop", serde_json::json!({})))
      .collect();
    let provider = Scripted::new("looper", rounds);
    let tools = registry_with(Vec::new());
    let mut trace = Recorder::default();

    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let report = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_max_requests(3)
    .run_turn("go", &CancelToken::new(), &mut SilentProgress)
    .unwrap();

    assert!(
      report.budget_exhausted,
      "the loop stopped on budget, not on an answer"
    );
    assert_eq!(report.status, TurnStatus::BudgetExhausted);
    assert_eq!(report.requests, 3);
    assert_eq!(
      provider.requests().len(),
      3,
      "the reserved finalization used the last request"
    );
    assert!(trace.kinds().iter().any(|kind| kind == "diagnostic"));
    let diagnostics = trace.0.lock().unwrap();
    assert!(diagnostics.iter().any(|(_, kind, payload)| {
      kind == "diagnostic"
        && payload["message"]
          .as_str()
          .unwrap_or_default()
          .contains("finalization requested tools")
    }));
  }

  #[test]
  fn finalization_is_one_request_and_exposes_no_tools() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tools = registry_with(vec![Box::new(Spy(Arc::clone(&seen)))]);
    let provider = Scripted::new("finalizer", vec![text("assessment")]);
    let mut trace = Recorder::default();
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let report = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_max_requests(32)
    .run_finalization("assess", &CancelToken::new(), &mut SilentProgress)
    .expect("finalization answer");

    assert_eq!(report.status, TurnStatus::Completed);
    assert_eq!(report.requests, 1);
    assert!(!report.budget_exhausted);
    assert_eq!(report.text, "assessment");
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].tools.is_empty(), "finalization exposed tools");
    assert!(seen.lock().unwrap().is_empty());
  }

  #[test]
  fn finalization_closes_an_unexpected_tool_call_without_executing_it() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tools = registry_with(vec![Box::new(Spy(Arc::clone(&seen)))]);
    let provider = Scripted::new(
      "toolish-finalizer",
      vec![tool_call("spy", serde_json::json!({}))],
    );
    let mut trace = Recorder::default();
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let mut runtime = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    );
    let report = runtime
      .run_finalization("assess", &CancelToken::new(), &mut SilentProgress)
      .expect("unexpected tool call is closed as an incomplete assessment");

    assert_eq!(report.status, TurnStatus::BudgetExhausted);
    assert!(report.budget_exhausted);
    assert!(
      seen.lock().unwrap().is_empty(),
      "finalization executed a tool"
    );
    assert_eq!(
      runtime
        .messages()
        .iter()
        .filter(|message| message.role == Role::Tool)
        .count(),
      1,
      "a finalization tool request still gets a model-visible terminal result"
    );
    drop(runtime);
    assert_eq!(trace.count("tool_failed"), 1);
  }

  /// A 20 000-token window on the balanced profile: compact at 15 000, checkpoint
  /// at 19 096, recent target (the eviction floor) at 5 000.
  const PRESSURED_WINDOW: u64 = 20_000;

  /// `turns` messages of exactly 1 000 estimated tokens each, first letter
  /// identifying them after an eviction.
  fn heavy_turns(turns: u32) -> Vec<Message> {
    (0..turns)
      .map(|turn| {
        let tag = (b'a' + turn as u8) as char;
        Message::user(format!("{tag}{}", "x".repeat(3_999)))
      })
      .collect()
  }

  #[test]
  fn a_compact_recommendation_evicts_oldest_turns_before_the_request() {
    let mut provider = Scripted::new("pressured", vec![text("ok")]);
    provider.capabilities.context_window = PRESSURED_WINDOW;
    let tools = registry_with(Vec::new());
    let policy =
      rupi_core::ProfilePolicy::new(rupi_core::ContextProfile::Balanced, PRESSURED_WINDOW);
    let mut trace = Recorder::default();

    TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_messages(heavy_turns(16))
    .run_turn("go", &CancelToken::new(), &mut SilentProgress)
    .expect("turn completes");

    // The exact request estimate includes the system guidance, so one more old
    // turn is removed than a message-only estimate would require.
    let request = &provider.requests()[0];
    assert_eq!(request.messages.len(), 5, "evicted to the recent target");
    assert!(request.messages[0].text().starts_with('m'));
    assert_eq!(request.messages[4].text(), "go");

    let kinds = trace.kinds();
    let reduced = kinds
      .iter()
      .position(|kind| kind == "context_reduced")
      .expect("the eviction is recorded");
    let started = kinds
      .iter()
      .position(|kind| kind == "model_request_started")
      .expect("the request still ran");
    assert!(
      reduced < started,
      "reduction precedes the request it serves"
    );
    let payload = trace.find("context_reduced").unwrap();
    assert_eq!(
      payload["reason"]["recent_target_exceeded"]["target_tokens"],
      5_000
    );
    assert!(
      payload["recovery_ref"].is_null(),
      "the recorder holds no blob store"
    );
  }

  #[test]
  fn a_second_eviction_inside_the_cooldown_warns_by_staying_put() {
    let mut provider = Scripted::new("pressured", vec![text("ok")]);
    provider.capabilities.context_window = PRESSURED_WINDOW;
    let tools = registry_with(Vec::new());
    let policy =
      rupi_core::ProfilePolicy::new(rupi_core::ContextProfile::Balanced, PRESSURED_WINDOW);
    let mut trace = Recorder::default();
    let mut runtime = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_messages(heavy_turns(16));
    // The policy's own cooldown, fed by the loop's last eviction: pressure again
    // right after compacting damps into a warning instead of a second eviction.
    runtime.last_compaction = Some(Instant::now());

    runtime
      .run_turn("go", &CancelToken::new(), &mut SilentProgress)
      .expect("turn completes");

    assert_eq!(provider.requests()[0].messages.len(), 17);
    assert_eq!(trace.count("context_reduced"), 0);
  }

  #[test]
  fn the_live_turn_is_never_evicted_and_the_recommendation_stays_visible() {
    let mut provider = Scripted::new("pressured", vec![text("ok")]);
    provider.capabilities.context_window = PRESSURED_WINDOW;
    let tools = registry_with(Vec::new());
    let policy =
      rupi_core::ProfilePolicy::new(rupi_core::ContextProfile::Balanced, PRESSURED_WINDOW);
    let mut trace = Recorder::default();

    // One prompt of 15 001 estimated tokens: over the compact threshold, and the
    // newest turn is the only turn. Nothing may be dropped, so the compaction
    // recommendation must surface as a warning rather than vanish.
    TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .run_turn(
      &"y".repeat(60_004),
      &CancelToken::new(),
      &mut SilentProgress,
    )
    .expect("an oversized turn is sent, not silently truncated");

    assert_eq!(provider.requests()[0].messages.len(), 1);
    assert_eq!(trace.count("context_reduced"), 0);
    assert!(
      trace
        .diagnostics()
        .iter()
        .any(|message| message.contains("compaction"))
    );
  }

  #[test]
  fn prefix_compaction_puts_summary_before_an_untouched_suffix() {
    let provider = Scripted::new("prefix", vec![text("ok")]);
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(rupi_core::ContextProfile::Balanced, 32_768);
    let mut trace = Recorder::default();
    let mut runtime = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_messages(vec![
      Message::user("m1"),
      Message::assistant("m2"),
      Message::user("m3"),
      Message::assistant("m4"),
    ]);
    let suffix = runtime.messages()[2..].to_vec();

    let replaced = runtime
      .compact_prefix(&TurnId::new(), 2, "summary")
      .expect("prefix compaction succeeds");

    assert_eq!(replaced, 2);
    assert_eq!(runtime.messages()[0], Message::user("summary"));
    assert_eq!(&runtime.messages()[1..], suffix.as_slice());
    assert_eq!(runtime.context_epoch, 1);
    let kinds = trace.kinds();
    let at = |kind: &str| kinds.iter().position(|item| item == kind).unwrap();
    assert!(
      at("context_compaction_started") < at("context_summary")
        && at("context_summary") < at("context_compaction_epoch")
        && at("context_compaction_epoch") < at("context_compaction_completed")
    );
    assert_eq!(trace.count("context_compaction_epoch"), 1);
  }

  #[test]
  fn durable_prefix_checkpoint_resume_keeps_the_current_turn_suffix() {
    let temp = rupi_store::TempDir::new("runtime-resume-prefix-checkpoint");
    let store = rupi_store::Store::open(temp.path(), rupi_store::WritePolicy::default())
      .expect("store opens");
    let session_id = SessionId::new();
    let model = ModelRef::new("test", "checkpoint-prefix");
    let session = store
      .begin(SessionHeader {
        session_id: session_id.clone(),
        version: rupi_core::session::SESSION_SCHEMA_VERSION,
        started_at_ms: 1,
        working_dir: "/workspace".into(),
        model: model.clone(),
        parent_session: None,
        branched_from_event: None,
        imported_from: None,
      })
      .expect("session begins");
    let provider = Scripted::new("checkpoint-prefix", vec![text("unused")]);
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(rupi_core::ContextProfile::Balanced, 32_768);
    let mut trace = StoreTrace::new(session);
    let mut runtime = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      session_id.clone(),
      TraceId::new(),
    );
    let turn = TurnId::new();
    for (text, is_current) in [("old history", false), ("current user", true)] {
      let message = Message::user(text);
      let envelope = runtime
        .emit_message(
          Some(turn.clone()),
          AgentEvent::UserMessage(UserMessage {
            text: text.into(),
            attachments: 0,
          }),
          &message,
        )
        .expect("user projection is durable");
      runtime.push_message(message, envelope.meta.seq);
      if is_current {
        // Keep the boundary at the first message of the active turn: the
        // checkpoint must summarize only the preceding prefix.
        assert_eq!(runtime.messages().len(), 2);
      }
    }
    let mut turn_history_start = 1;
    runtime
      .checkpoint_turn_prefix(
        &turn,
        ContextCapsule::new("prefix checkpoint"),
        &mut turn_history_start,
      )
      .expect("prefix checkpoint succeeds");
    assert_eq!(
      runtime
        .messages()
        .iter()
        .map(Message::text)
        .collect::<Vec<_>>(),
      [
        "[Session Checkpoint Capsule]\nobjective: prefix checkpoint\n[/Session Checkpoint Capsule]",
        "current user"
      ]
    );
    // Prove the first boundary before writing a second one. The restored
    // projection contains the suffix but not the protected capsule message.
    runtime.trace.flush().expect("flush first checkpoint");
    let first = store
      .restore(&session_id)
      .expect("restore first checkpoint");
    assert_eq!(first.summarized_messages, 1);
    assert_eq!(first.messages[0].message.text(), "current user");
    assert_eq!(first.checkpoint.unwrap().objective, "prefix checkpoint");

    // A subsequent prefix checkpoint must count only semantic tail messages;
    // the old capsule is protected in memory but absent from the session log.
    let second = Message::user("second current user");
    let second_envelope = runtime
      .emit_message(
        Some(turn.clone()),
        AgentEvent::UserMessage(UserMessage {
          text: "second current user".into(),
          attachments: 0,
        }),
        &second,
      )
      .expect("second user projection is durable");
    runtime.push_message(second, second_envelope.meta.seq);
    let mut second_turn_start = 2;
    runtime
      .checkpoint_turn_prefix(
        &turn,
        ContextCapsule::new("second prefix checkpoint"),
        &mut second_turn_start,
      )
      .expect("second prefix checkpoint succeeds");
    drop(runtime);
    trace.flush().expect("flush durable state");
    drop(trace);

    let restored = store
      .restore(&session_id)
      .expect("restore second checkpoint");
    assert_eq!(restored.summarized_messages, 2);
    assert_eq!(restored.messages[0].message.text(), "second current user");
    assert_eq!(
      restored.checkpoint.unwrap().objective,
      "second prefix checkpoint"
    );
  }

  #[test]
  fn durable_l0_eviction_resume_keeps_the_same_model_visible_suffix() {
    let temp = rupi_store::TempDir::new("runtime-resume-l0");
    let store = rupi_store::Store::open(temp.path(), rupi_store::WritePolicy::default())
      .expect("store opens");
    let session_id = SessionId::new();
    let model = ModelRef::new("test", "l0");
    let session = store
      .begin(SessionHeader {
        session_id: session_id.clone(),
        version: rupi_core::session::SESSION_SCHEMA_VERSION,
        started_at_ms: 1,
        working_dir: "/workspace".into(),
        model: model.clone(),
        parent_session: None,
        branched_from_event: None,
        imported_from: None,
      })
      .expect("session begins");
    let provider = Scripted::new("l0", vec![text("answer 1"), text("answer 2")]);
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(rupi_core::ContextProfile::Balanced, 32_768);
    let mut trace = StoreTrace::new(session);
    let mut runtime = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      session_id.clone(),
      TraceId::new(),
    );
    runtime
      .run_turn("question 1", &CancelToken::new(), &mut SilentProgress)
      .expect("first turn");
    runtime
      .run_turn("question 2", &CancelToken::new(), &mut SilentProgress)
      .expect("second turn");
    let target = estimate_tokens(&runtime.assemble_request(runtime.messages()[2..].to_vec()));
    let mut turn_start = runtime.messages().len();
    let removed = runtime
      .evict_oldest(target, &TurnId::new(), &mut turn_start)
      .expect("eviction succeeds");
    assert_eq!(removed, 2);
    let expected = runtime
      .messages()
      .iter()
      .map(Message::text)
      .collect::<Vec<_>>();
    drop(runtime);
    trace.flush().expect("flush durable state");
    drop(trace);

    let restored = store.restore(&session_id).expect("restore state");
    assert_eq!(restored.reductions.len(), 1);
    assert_eq!(
      restored
        .messages
        .iter()
        .map(|m| m.message.text())
        .collect::<Vec<_>>(),
      expected
    );
  }

  #[test]
  fn durable_compaction_resume_reuses_the_reduced_window() {
    let temp = rupi_store::TempDir::new("runtime-resume-compaction");
    let store = rupi_store::Store::open(temp.path(), rupi_store::WritePolicy::default())
      .expect("store opens");
    let session_id = SessionId::new();
    let model = ModelRef::new("test", "resume");
    let session = store
      .begin(SessionHeader {
        session_id: session_id.clone(),
        version: rupi_core::session::SESSION_SCHEMA_VERSION,
        started_at_ms: 1,
        working_dir: "/workspace".into(),
        model: model.clone(),
        parent_session: None,
        branched_from_event: None,
        imported_from: None,
      })
      .expect("session begins");
    let provider = Scripted::new(
      "resume",
      vec![text("first"), text("second"), text("continued")],
    );
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(rupi_core::ContextProfile::Balanced, 32_768);
    let mut trace = StoreTrace::new(session);
    let mut runtime = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      session_id.clone(),
      TraceId::new(),
    );
    runtime
      .run_turn("first question", &CancelToken::new(), &mut SilentProgress)
      .expect("first turn");
    runtime
      .run_turn("second question", &CancelToken::new(), &mut SilentProgress)
      .expect("second turn");
    runtime
      .compact(&TurnId::new(), "summary of the first turn", 1)
      .expect("compaction");
    drop(runtime);
    trace.flush().expect("flush durable state");
    drop(trace);

    let restored = store.restore(&session_id).expect("restore state");
    assert_eq!(restored.context_epoch, 1);
    assert_eq!(restored.epochs.len(), 1);
    assert_eq!(
      restored
        .messages
        .iter()
        .map(|message| message.message.text())
        .collect::<Vec<_>>(),
      ["summary of the first turn", "second"]
    );

    let session = store.resume(&session_id).expect("reopen session");
    let mut resumed_trace = StoreTrace::new(session);
    let resume_provider = Scripted::new("resume", vec![text("continued")]);
    let resume_epoch = ModelEpoch {
      index: restored.epochs[0].epoch,
      model: restored.epochs[0].model.clone(),
      capabilities: resume_provider.capabilities(),
      reason: restored.epochs[0].reason.clone(),
      started_by_event: None,
    };
    let state = ResumeState {
      messages: restored
        .messages
        .iter()
        .map(|message| message.message.clone())
        .collect(),
      message_seqs: restored
        .messages
        .iter()
        .map(|message| message.seq)
        .collect(),
      epochs: vec![resume_epoch],
      context_epoch: restored.context_epoch,
      checkpoint_floor: 0,
      cited_history: restored.last_seq.map(|last| (EventSeq(1), last)),
      interrupted_tools: restored.interrupted_tools.clone(),
    };
    let mut resumed = TurnLoop::new(
      &resume_provider,
      &tools,
      &policy,
      &mut resumed_trace,
      session_id,
      TraceId::new(),
    )
    .with_resume_state(state)
    .expect("resume state validates");
    resumed
      .run_turn("continue", &CancelToken::new(), &mut SilentProgress)
      .expect("resumed turn");
    assert_eq!(
      resume_provider.requests()[0].messages[0].text(),
      "summary of the first turn"
    );
    assert_eq!(resume_provider.requests()[0].messages[1].text(), "second");
  }

  #[test]
  fn resumed_tool_lifecycle_is_reconciled_before_the_provider_request() {
    let temp = rupi_store::TempDir::new("runtime-resume-tool");
    let target = temp.child("already-written.txt");
    std::fs::write(&target, "committed").unwrap();
    let provider = Scripted::new("resume-tool", vec![text("continue")]);
    let tools = ToolRegistry::new(Workspace::new(temp.path()).unwrap())
      .with_policy(&ToolPolicy {
        auto_approve_mutating: true,
        ..ToolPolicy::default()
      })
      .with_builtins();
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let old_turn = TurnId::new();
    let pending = rupi_core::InterruptedToolCall {
      request: rupi_core::ToolRequest {
        call_id: rupi_core::ToolCallId::new(),
        name: "write".into(),
        arguments: serde_json::json!({
          "path": "already-written.txt",
          "contents": "committed"
        }),
      },
      state: ToolExecutionState::Started,
      read_only: false,
      turn_id: Some(old_turn),
      epoch: Some(0),
      model: Some(provider.model().clone()),
      request_event_id: None,
      started_event_id: None,
    };
    let mut trace = Recorder::default();
    let mut runtime = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_resume_state(ResumeState {
      messages: vec![Message::user("prior question")],
      message_seqs: vec![None],
      epochs: vec![ModelEpoch {
        index: 0,
        model: provider.model().clone(),
        capabilities: provider.capabilities(),
        reason: EpochReason::Initial,
        started_by_event: None,
      }],
      context_epoch: 0,
      checkpoint_floor: 0,
      cited_history: None,
      interrupted_tools: vec![pending],
    })
    .expect("resume state validates");

    runtime
      .run_turn("new question", &CancelToken::new(), &mut SilentProgress)
      .expect("reconciled session continues");
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(trace.count("tool_completed"), 1);
    assert!(
      trace
        .kinds()
        .iter()
        .position(|kind| kind == "tool_completed")
        .zip(
          trace
            .kinds()
            .iter()
            .position(|kind| kind == "model_request_started")
        )
        .is_some_and(|(reconciled, request)| reconciled < request),
      "reconciliation must precede the first provider request"
    );
  }

  #[test]
  fn resumed_manual_tool_reconciliation_blocks_provider_contact() {
    let provider = Scripted::new("resume-manual", vec![text("must not run")]);
    let temp = rupi_store::TempDir::new("runtime-resume-manual-tool");
    let tools = ToolRegistry::new(Workspace::new(temp.path()).unwrap())
      .with_policy(&ToolPolicy {
        auto_approve_mutating: true,
        ..ToolPolicy::default()
      })
      .with_builtins();
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let mut trace = Recorder::default();
    let mut runtime = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_resume_state(ResumeState {
      messages: vec![],
      message_seqs: vec![],
      epochs: vec![ModelEpoch {
        index: 0,
        model: provider.model().clone(),
        capabilities: provider.capabilities(),
        reason: EpochReason::Initial,
        started_by_event: None,
      }],
      context_epoch: 0,
      checkpoint_floor: 0,
      cited_history: None,
      interrupted_tools: vec![rupi_core::InterruptedToolCall {
        request: rupi_core::ToolRequest {
          call_id: rupi_core::ToolCallId::new(),
          name: "exec".into(),
          arguments: serde_json::json!({"command": "echo unsafe"}),
        },
        state: ToolExecutionState::Started,
        read_only: false,
        turn_id: Some(TurnId::new()),
        epoch: Some(0),
        model: Some(provider.model().clone()),
        request_event_id: None,
        started_event_id: None,
      }],
    })
    .expect("resume state validates");
    let error = runtime
      .run_turn("new question", &CancelToken::new(), &mut SilentProgress)
      .unwrap_err();
    assert!(matches!(error, TurnError::Sink(message) if message.contains("manual inspection")));
    assert!(provider.requests().is_empty());
    let second = runtime
      .run_turn(
        "must still be blocked",
        &CancelToken::new(),
        &mut SilentProgress,
      )
      .unwrap_err();
    assert!(matches!(second, TurnError::Sink(message) if message.contains("still unresolved")));
    assert!(provider.requests().is_empty());
  }

  #[test]
  fn a_checkpoint_can_be_restored_even_when_it_is_the_first_durable_event() {
    let temp = rupi_store::TempDir::new("runtime-first-checkpoint");
    let store = rupi_store::Store::open(temp.path(), rupi_store::WritePolicy::default())
      .expect("store opens");
    let session_id = SessionId::new();
    let model = ModelRef::new("test", "checkpoint");
    let session = store
      .begin(SessionHeader {
        session_id: session_id.clone(),
        version: rupi_core::session::SESSION_SCHEMA_VERSION,
        started_at_ms: 1,
        working_dir: "/workspace".into(),
        model: model.clone(),
        parent_session: None,
        branched_from_event: None,
        imported_from: None,
      })
      .expect("session begins");
    let provider = Scripted::new("checkpoint", vec![text("unused")]);
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(rupi_core::ContextProfile::Balanced, 32_768);
    let mut trace = StoreTrace::new(session);
    let mut runtime = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      session_id.clone(),
      TraceId::new(),
    );
    runtime
      .checkpoint(&TurnId::new(), ContextCapsule::new("first checkpoint"))
      .expect("checkpoint succeeds");
    drop(runtime);
    trace.flush().expect("flush durable state");
    drop(trace);

    let restored = store.restore(&session_id).expect("restore state");
    assert_eq!(
      restored.checkpoint.as_ref().unwrap().objective,
      "first checkpoint"
    );
    assert_eq!(restored.context_epoch, 1);
  }

  #[test]
  fn ordinary_compaction_never_crosses_a_checkpoint_floor() {
    let provider = Scripted::new("checkpoint-floor", vec![text("ok")]);
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(rupi_core::ContextProfile::Balanced, 32_768);
    let mut trace = Recorder::default();
    let mut runtime = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    );
    runtime
      .checkpoint(&TurnId::new(), ContextCapsule::new("checkpoint objective"))
      .expect("checkpoint succeeds");
    runtime.messages_mut().extend([
      Message::user("after checkpoint"),
      Message::assistant("tail"),
    ]);

    let removed = runtime
      .compact(&TurnId::new(), "post-checkpoint summary", 1)
      .expect("ordinary compaction succeeds");
    assert_eq!(removed, 1);
    assert!(
      runtime.messages()[0]
        .text()
        .contains("objective: checkpoint objective")
    );
    assert_eq!(runtime.messages()[1].text(), "post-checkpoint summary");
    assert_eq!(runtime.messages()[2].text(), "tail");
    assert_eq!(runtime.checkpoint_floor, 1);
    let completions = trace.all("context_compaction_completed");
    let completed = completions
      .last()
      .expect("ordinary completion recorded after the checkpoint");
    assert_eq!(completed["retained_messages"], 1);
    assert_eq!(completed["removed_messages"], 1);
  }

  #[test]
  fn summary_compaction_with_only_a_checkpoint_tail_is_a_noop() {
    let provider = Scripted::new("checkpoint-only-tail", vec![text("ok")]);
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(rupi_core::ContextProfile::Balanced, 32_768);
    let mut trace = Recorder::default();
    let mut runtime = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    );
    runtime
      .checkpoint(&TurnId::new(), ContextCapsule::new("checkpoint objective"))
      .expect("checkpoint succeeds");
    runtime
      .messages_mut()
      .push(Message::user("one tail message"));
    assert_eq!(
      runtime
        .compact_with_summary_or(&TurnId::new(), 1, None)
        .expect("no-op compaction succeeds"),
      0
    );
    assert_eq!(runtime.messages().len(), 2);
    assert_eq!(trace.count("context_compaction_completed"), 1);
  }

  #[test]
  fn compaction_opens_a_durable_epoch_and_leaves_the_summary_visible() {
    let provider = Scripted::new("compacted", vec![text("ok")]);
    let tools = registry_with(Vec::new());
    let policy =
      rupi_core::ProfilePolicy::new(rupi_core::ContextProfile::Balanced, PRESSURED_WINDOW);
    let mut trace = Recorder::default();
    let harness = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    );

    let turn = TurnId::new();
    // The loop starts from a resumed session: two durable user messages at
    // journal positions one and two, neither of which this process replayed.
    let mut harness = harness.with_cited_history(rupi_core::EventSeq(1), rupi_core::EventSeq(2));
    harness
      .run_turn("first question", &CancelToken::new(), &mut SilentProgress)
      .expect("the round completes");
    harness
      .messages_mut()
      .retain(|message| message.text() != "ok");
    let removed = harness
      .compact(&turn, "the user asked about X; nothing answered yet", 1)
      .expect("compaction records");
    assert_eq!(removed, 1, "one message left the visible window");
    assert_eq!(
      harness
        .messages()
        .iter()
        .map(Message::text)
        .collect::<Vec<_>>(),
      ["the user asked about X; nothing answered yet"],
      "the summary is the whole visible history"
    );
    assert_eq!(harness.context_epoch, 1, "the first epoch is epoch one");

    let kinds = trace.kinds();
    let position = |wanted: &str| kinds.iter().position(|kind| *kind == wanted);
    let started = position("context_compaction_started").expect("a start is recorded");
    let summary = position("context_summary").expect("the summary is an event");
    let epoch = position("context_compaction_epoch").expect("the epoch is durable");
    let completed = position("context_compaction_completed").expect("a completion is recorded");
    assert!(
      started < summary && summary < epoch && epoch < completed,
      "start, summary, epoch, completion: {kinds:?}"
    );

    let epoch_payload = trace.find("context_compaction_epoch").unwrap();
    assert_eq!(epoch_payload["context_epoch"], 1);
    assert!(
      epoch_payload["summary"].is_null(),
      "the recorder holds no blob store, so no recovery reference exists"
    );
    let completed_payload = trace.find("context_compaction_completed").unwrap();
    assert_eq!(completed_payload["removed_messages"], 1);
    assert_eq!(completed_payload["retained_messages"], 0);
  }

  #[test]
  fn a_second_compaction_claims_the_next_epoch() {
    // Three real rounds: each answer is an emitted envelope, so the epoch can
    // name journal positions the way a durable trace would.
    let provider = Scripted::new(
      "twice",
      vec![text("one done"), text("two done"), text("three done")],
    );
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let mut trace = Recorder::default();
    let mut harness = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    );

    let turn = TurnId::new();
    for label in ["one", "two", "three"] {
      harness
        .run_turn(label, &CancelToken::new(), &mut SilentProgress)
        .expect("the round completes");
    }
    // Six live messages: three prompts, three answers. Keep one.
    let removed = harness.compact(&turn, "a summary", 1).unwrap();
    assert_eq!(removed, 5, "everything but the newest message left");
    assert_eq!(
      harness
        .messages()
        .iter()
        .map(Message::text)
        .collect::<Vec<_>>(),
      ["a summary", "three done"],
      "the summary precedes the one message retained across it"
    );
    let removed = harness.compact(&turn, "a summary of a summary", 0).unwrap();
    assert_eq!(
      removed, 2,
      "summary plus the one retained message compact again"
    );
    assert_eq!(
      harness
        .messages()
        .iter()
        .map(Message::text)
        .collect::<Vec<_>>(),
      ["a summary of a summary"],
      "retaining nothing leaves only the second summary"
    );

    let epochs = trace.all("context_compaction_epoch");
    assert_eq!(epochs.len(), 2);
    assert_eq!(epochs[0]["context_epoch"], 1);
    assert_eq!(
      epochs[1]["context_epoch"], 2,
      "epochs are numbered in order"
    );
  }

  #[test]
  fn a_durable_epoch_names_the_journal_range_it_replaces() {
    // A store-backed trace stamps real journal positions, so the epoch record
    // can be checked against the coordinates a reader must honor on restore.
    let temp = rupi_store::TempDir::new("compaction-epoch");
    let store = rupi_store::Store::open(temp.path(), rupi_store::WritePolicy::default())
      .expect("store opens");
    let session_id = SessionId::new();
    let model = rupi_core::ModelRef::new("test", "durable");
    let session = store
      .begin(rupi_core::SessionHeader {
        session_id: session_id.clone(),
        version: rupi_core::session::SESSION_SCHEMA_VERSION,
        started_at_ms: 1,
        working_dir: "/workspace".into(),
        model: model.clone(),
        parent_session: None,
        branched_from_event: None,
        imported_from: None,
      })
      .expect("session begins");
    let provider = Scripted::new("durable", vec![text("done once")]);
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(
      rupi_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let mut trace = StoreTrace::new(session);
    let mut harness = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      session_id.clone(),
      TraceId::new(),
    );

    let turn = TurnId::new();
    harness
      .run_turn("the question", &CancelToken::new(), &mut SilentProgress)
      .expect("the round completes");
    let removed = harness
      .compact(&turn, "the question, unanswered", 0)
      .expect("compaction records");
    assert_eq!(removed, 2, "the prompt and its answer were replaced");
    trace.flush().expect("the trace is durable");

    // Read the durable journal rather than the live trace: the coordinates a
    // future restore must honor are what the journal recorded.
    let journal = rupi_store::TraceJournal::read(trace.session().trace_path())
      .expect("the journal is readable");
    let epoch = journal
      .items
      .iter()
      .find_map(|item| match &item.envelope.event {
        AgentEvent::ContextCompactionEpoch(record) => {
          Some(serde_json::to_value(record).expect("the record serializes"))
        }
        _ => None,
      })
      .expect("the epoch is durable");
    assert_eq!(epoch["context_epoch"], 1);
    assert_eq!(
      epoch["replaces_from"], 3,
      "the summary replaces the first model-visible message"
    );
    // The user message and assistant answer are the only model-visible records
    // replaced; request/lifecycle events and the retained suffix are excluded.
    // The assistant projection is bound to the terminal completion event so its
    // tool-call payload and text share one recovery transaction.
    assert_eq!(epoch["replaces_through"], 6);
    let summary = epoch["summary"]
      .as_object()
      .expect("the epoch carries a stored blob reference, not prose");
    assert!(
      summary["hash"]
        .as_str()
        .is_some_and(|hash| hash.len() == 64),
      "the reference names stored bytes by digest: {summary:?}"
    );

    let restored = store.restore(&session_id).expect("resume projection reads");
    assert_eq!(restored.epochs.len(), 1);
    assert_eq!(restored.epochs[0].epoch, 0);
    assert_eq!(restored.compactions.len(), 1);
    assert_eq!(restored.compactions[0].replaces_from, Some(EventSeq(3)));
    assert_eq!(restored.compactions[0].replaces_through, Some(EventSeq(6)));
    assert_eq!(restored.context_epoch, 1);
    assert_eq!(
      restored
        .messages
        .iter()
        .map(|message| message.message.text())
        .collect::<Vec<_>>(),
      ["the question, unanswered"]
    );
  }

  #[test]
  fn summarizing_compaction_producer_triggers_under_window_pressure() {
    let mut provider = Scripted::new("pressured", vec![text("ok")]);
    provider.capabilities.context_window = PRESSURED_WINDOW;
    let tools = registry_with(Vec::new());
    let policy =
      rupi_core::ProfilePolicy::new(rupi_core::ContextProfile::Balanced, PRESSURED_WINDOW);
    let mut trace = Recorder::default();

    TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_messages(heavy_turns(16))
    .with_compaction_strategy(CompactionStrategy::Summarize)
    .run_turn("go", &CancelToken::new(), &mut SilentProgress)
    .expect("turn completes");

    let kinds = trace.kinds();
    let started = kinds.iter().position(|k| k == "context_compaction_started");
    let summary = kinds.iter().position(|k| k == "context_summary");
    let epoch = kinds.iter().position(|k| k == "context_compaction_epoch");
    let completed = kinds
      .iter()
      .position(|k| k == "context_compaction_completed");

    assert!(
      started.is_some(),
      "compaction started is recorded: {kinds:?}"
    );
    assert!(summary.is_some(), "summary message is recorded: {kinds:?}");
    assert!(epoch.is_some(), "epoch is opened: {kinds:?}");
    assert!(
      completed.is_some(),
      "compaction completed is recorded: {kinds:?}"
    );

    let epoch_payload = trace.find("context_compaction_epoch").unwrap();
    assert_eq!(epoch_payload["context_epoch"], 1);

    // The model request that ran carried the summary and the retained messages.
    let req = &provider.requests()[0];
    assert!(
      req
        .messages
        .iter()
        .any(|m| m.text().contains("Summary of earlier conversation")),
      "request carries the synthesized summary"
    );
  }

  #[test]
  fn provider_overflow_does_not_open_a_second_epoch_after_proactive_compaction() {
    let mut provider = Scripted::new("proactive-overflow", vec![text("ok")]).fails(
      0,
      ModelFailure::new(
        ModelFailureKind::ContextOverflow,
        FailurePhase::WaitingForResponse,
        "context window exceeded",
      ),
    );
    provider.capabilities.context_window = PRESSURED_WINDOW;
    let tools = registry_with(Vec::new());
    let policy =
      rupi_core::ProfilePolicy::new(rupi_core::ContextProfile::Balanced, PRESSURED_WINDOW);
    let mut trace = Recorder::default();

    let report = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_messages(heavy_turns(16))
    .with_compaction_strategy(CompactionStrategy::Summarize)
    .run_turn("go", &CancelToken::new(), &mut SilentProgress)
    .expect("the existing proactive compaction is reused");

    assert_eq!(report.requests, 2);
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(trace.count("context_compaction_epoch"), 1);
  }

  #[test]
  fn custom_summarizer_is_honoured_during_compaction() {
    let mut provider = Scripted::new("pressured", vec![text("ok")]);
    provider.capabilities.context_window = PRESSURED_WINDOW;
    let tools = registry_with(Vec::new());
    let policy =
      rupi_core::ProfilePolicy::new(rupi_core::ContextProfile::Balanced, PRESSURED_WINDOW);
    let mut trace = Recorder::default();

    TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_messages(heavy_turns(16))
    .with_summarizer(|slice| format!("custom capsule of {} turns", slice.len()))
    .run_turn("go", &CancelToken::new(), &mut SilentProgress)
    .expect("turn completes");

    let req = &provider.requests()[0];
    assert!(
      req
        .messages
        .iter()
        .any(|m| m.text().contains("custom capsule")),
      "request carries the custom summary"
    );
  }

  #[test]
  fn external_context_retrieved_is_recorded_and_folded_into_turn() {
    let provider = Scripted::new("primary", vec![text("read external context successfully")]);
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(rupi_core::ContextProfile::Balanced, 64_000);
    let mut trace = Recorder::default();

    let external_source = rupi_core::trace::ExternalContextSource {
      provider: "rkb-rs".into(),
      resource_id: "doc-123".into(),
      provenance: "rkb-rs/citation".into(),
    };

    let item = ExternalContextItem::inline(
      external_source,
      "Key knowledge: rupi is written in Rust 2024.",
      Some("RFC-001".into()),
    );

    TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .run_turn_with_external_context(
      "what is the key knowledge?",
      &[item],
      &CancelToken::new(),
      &mut SilentProgress,
    )
    .expect("turn completes");

    let kinds = trace.kinds();
    assert!(
      kinds.contains(&"external_context_retrieved".to_string()),
      "trace records external_context_retrieved event: {kinds:?}"
    );

    let ext_payload = trace.find("external_context_retrieved").unwrap();
    assert_eq!(ext_payload["source"]["provider"], "rkb-rs");
    assert_eq!(ext_payload["source"]["resource_id"], "doc-123");
    assert_eq!(ext_payload["citation"], "RFC-001");
    assert_eq!(ext_payload["inline"], true);

    let req = &provider.requests()[0];
    assert!(
      req.messages.iter().any(|m| m
        .text()
        .contains("External context from rkb-rs/citation:rkb-rs:doc-123 (RFC-001)")
        && m.text().contains("rupi is written in Rust 2024")),
      "model request carries the folded external context: {:?}",
      req.messages
    );
  }

  #[test]
  fn checkpoint_creation_driven_by_runtime_under_context_pressure() {
    let mut provider = Scripted::new("pressured", vec![text("ok")]);
    provider.capabilities.context_window = PRESSURED_WINDOW;
    let tools = registry_with(Vec::new());
    let mut policy =
      rupi_core::ProfilePolicy::new(rupi_core::ContextProfile::Balanced, PRESSURED_WINDOW);
    policy.thresholds.checkpoint_tokens = 1_000;
    let mut trace = Recorder::default();

    TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_messages(heavy_turns(10))
    .run_turn("start turn", &CancelToken::new(), &mut SilentProgress)
    .expect("turn completes");

    let kinds = trace.kinds();
    assert!(
      kinds.contains(&"checkpoint_created".to_string()),
      "trace records checkpoint_created event: {kinds:?}"
    );
    assert!(
      kinds.contains(&"context_compaction_completed".to_string()),
      "trace records context_compaction_completed event: {kinds:?}"
    );

    let cp_payload = trace.find("checkpoint_created").unwrap();
    assert_eq!(cp_payload["capsule_version"], 1);

    let req = &provider.requests()[0];
    assert!(
      req
        .messages
        .iter()
        .any(|m| m.text().contains("[Session Checkpoint Capsule]")),
      "model request carries the formatted checkpoint capsule"
    );
  }

  #[test]
  fn custom_checkpointer_is_honoured_under_context_pressure() {
    let mut provider = Scripted::new("pressured", vec![text("ok")]);
    provider.capabilities.context_window = PRESSURED_WINDOW;
    let tools = registry_with(Vec::new());
    let mut policy =
      rupi_core::ProfilePolicy::new(rupi_core::ContextProfile::Balanced, PRESSURED_WINDOW);
    policy.thresholds.checkpoint_tokens = 1_000;
    let mut trace = Recorder::default();

    TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_messages(heavy_turns(10))
    .with_checkpointer(|_msgs, _state| {
      let mut cap = ContextCapsule::new("custom objective from hook");
      cap.current_state = "custom state hook".into();
      cap
    })
    .run_turn("start turn", &CancelToken::new(), &mut SilentProgress)
    .expect("turn completes");

    let req = &provider.requests()[0];
    assert!(
      req
        .messages
        .iter()
        .any(|m| m.text().contains("objective: custom objective from hook")),
      "model request carries the custom checkpointer output"
    );
  }

  #[test]
  fn manual_failover_transitions_epoch_and_activates_backup() {
    let primary = Scripted::new("primary", vec![text("primary ok")]);
    let backup = Scripted::new("backup", vec![text("backup ok")]);
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(rupi_core::ContextProfile::Balanced, 32_768);
    let mut trace = Recorder::default();

    let mut turn_loop = TurnLoop::new(
      &primary,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_backup(&backup);

    assert_eq!(turn_loop.active_model(), *primary.model());
    assert!(!turn_loop.failed_over());

    let epoch = turn_loop
      .failover_manual()
      .expect("manual failover succeeds");
    assert_eq!(epoch.index, 1);
    assert_eq!(epoch.model, *backup.model());
    assert_eq!(epoch.reason, EpochReason::ManualSwitch);
    assert_eq!(turn_loop.active_model(), *backup.model());
    assert!(turn_loop.failed_over());

    // Next turn is executed against backup provider
    turn_loop
      .run_turn("hello backup", &CancelToken::new(), &mut SilentProgress)
      .expect("turn succeeds");
    assert_eq!(backup.requests().len(), 1);
    assert_eq!(primary.requests().len(), 0);

    // Switch back to primary
    let epoch2 = turn_loop
      .switch_back_manual()
      .expect("switch back succeeds");
    assert_eq!(epoch2.index, 2);
    assert_eq!(epoch2.model, *primary.model());
    assert_eq!(epoch2.reason, EpochReason::ManualSwitchBack);
    assert_eq!(turn_loop.active_model(), *primary.model());
    assert!(!turn_loop.failed_over());

    // Next turn is executed against primary provider
    turn_loop
      .run_turn("hello primary", &CancelToken::new(), &mut SilentProgress)
      .expect("turn succeeds");
    assert_eq!(primary.requests().len(), 1);
    assert_eq!(backup.requests().len(), 1);
  }

  #[test]
  fn resuming_restores_the_active_epoch_and_lifecycle_identity() {
    let primary = Scripted::new("primary", Vec::new());
    let backup = Scripted::new("backup", vec![text("continued")]);
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(rupi_core::ContextProfile::Balanced, 32_768);
    let mut trace = Recorder::default();
    let primary_epoch = ModelEpoch {
      index: 0,
      model: primary.model().clone(),
      capabilities: primary.capabilities(),
      reason: EpochReason::Initial,
      started_by_event: None,
    };
    let backup_epoch = ModelEpoch {
      index: 1,
      model: backup.model().clone(),
      capabilities: backup.capabilities(),
      reason: EpochReason::AutomaticFailover,
      started_by_event: None,
    };
    let state = ResumeState {
      messages: vec![Message::user("durable history")],
      message_seqs: vec![None],
      epochs: vec![primary_epoch, backup_epoch],
      context_epoch: 3,
      checkpoint_floor: 0,
      cited_history: Some((rupi_core::EventSeq(1), rupi_core::EventSeq(17))),
      interrupted_tools: Vec::new(),
    };

    let mut turn_loop = TurnLoop::new(
      &primary,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_backup(&backup)
    .with_resume_state(state)
    .expect("the configured backup can resume the active epoch");

    assert_eq!(turn_loop.active_model(), *backup.model());
    turn_loop
      .run_turn("continue", &CancelToken::new(), &mut SilentProgress)
      .expect("resumed turn succeeds");
    assert!(primary.requests().is_empty());
    assert_eq!(backup.requests().len(), 1);
    assert!(
      backup.requests()[0]
        .messages
        .iter()
        .any(|message| message.text() == "durable history")
    );

    let started = trace.all("session_started");
    assert_eq!(started.len(), 1);
    assert_eq!(started[0]["resumed"], true);
    assert_eq!(
      started[0]["model"],
      serde_json::to_value(backup.model()).expect("model serializes")
    );
    assert_eq!(trace.count("model_epoch_started"), 0);
  }

  #[test]
  fn manual_failover_refused_without_backup() {
    let primary = Scripted::new("primary", vec![text("primary ok")]);
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(rupi_core::ContextProfile::Balanced, 32_768);
    let mut trace = Recorder::default();

    let mut turn_loop = TurnLoop::new(
      &primary,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    );

    let err = turn_loop.failover_manual().unwrap_err();
    assert!(
      matches!(err, TurnError::Refused(message) if message.contains("no backup model configured"))
    );
  }

  #[test]
  fn retry_backoff_respects_cancellation() {
    let failure = ModelFailure::new(
      ModelFailureKind::Transport,
      FailurePhase::Streaming,
      "connection dropped",
    );
    let provider = Scripted::new("retry_cancel", vec![text("ok")]).fails(0, failure);
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(rupi_core::ContextProfile::Balanced, 32_768);
    let mut trace = Recorder::default();

    let cancel = CancelToken::new();
    cancel.cancel();

    let mut turn_loop = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    );

    let res = turn_loop.run_turn("test", &cancel, &mut SilentProgress);
    assert!(res.is_ok());
    let report = res.unwrap();
    assert!(matches!(report.status, TurnStatus::Cancelled));
  }

  #[test]
  fn compact_phase_emits_l2_events_and_retains_latest_message() {
    let provider = Scripted::new("phase", vec![text("phase ok")]);
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(rupi_core::ContextProfile::Balanced, 32_768);
    let mut trace = Recorder::default();

    let mut turn_loop = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_messages(vec![
      Message::user("step 1: design spec"),
      Message::assistant("spec completed"),
      Message::user("step 2: implement code"),
      Message::assistant("code implemented"),
      Message::user("step 3: write tests"),
    ]);

    let turn_id = TurnId::new();
    let removed = turn_loop
      .compact_phase(&turn_id, "implementation complete", None, true)
      .expect("compact phase succeeds");

    assert_eq!(removed, 4);
    assert_eq!(turn_loop.messages().len(), 2); // 1 retained + 1 phase summary message
    assert!(
      turn_loop.messages()[0]
        .text()
        .contains("[Phase Compaction: implementation complete]")
    );

    let kinds = trace.kinds();
    assert!(kinds.contains(&"context_compaction_started".to_string()));
    assert!(kinds.contains(&"context_compaction_completed".to_string()));

    let started = trace.find("context_compaction_started").unwrap();
    assert_eq!(started["level"], "l2_phase");
    assert_eq!(started["reason"], "semantic phase: implementation complete");

    let completed = trace.find("context_compaction_completed").unwrap();
    assert_eq!(completed["level"], "l2_phase");
    assert_eq!(completed["removed_messages"], 4);
    assert_eq!(completed["retained_messages"], 1);
  }

  #[test]
  fn compact_phase_cooldown_and_force_gate() {
    let provider = Scripted::new("phase", vec![text("ok")]);
    let tools = registry_with(Vec::new());
    let policy = rupi_core::ProfilePolicy::new(rupi_core::ContextProfile::Balanced, 32_768);
    let mut trace = Recorder::default();

    let mut turn_loop = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    )
    .with_messages(vec![
      Message::user("m1"),
      Message::assistant("m2"),
      Message::user("m3"),
    ]);

    let turn_id = TurnId::new();
    let removed = turn_loop
      .compact_phase(&turn_id, "phase 1", None, true)
      .expect("first phase succeeds");
    assert_eq!(removed, 2);

    // Immediate second phase compaction without force is deferred by cooldown
    turn_loop.messages_mut().push(Message::user("m4"));
    turn_loop.messages_mut().push(Message::assistant("m5"));
    let removed2 = turn_loop
      .compact_phase(&turn_id, "phase 2", None, false)
      .expect("second phase without force");
    assert_eq!(removed2, 0);

    // With force: true, cooldown is bypassed
    let removed3 = turn_loop
      .compact_phase(&turn_id, "phase 2", None, true)
      .expect("second phase with force");
    assert_eq!(removed3, 3);
  }
}
