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

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use pi_rs_core::{
  AgentEvent, AssistantDelta, AttributedMessage, BlobRef, CAPSULE_SCHEMA_VERSION, CancelToken,
  CapabilityGap, CapsuleArtifact, CapsuleDecision, CheckpointCreated, CheckpointId, ContentBlock,
  ContextAction, ContextCapsule, ContextCompactionCompleted, ContextCompactionEpoch,
  ContextCompactionStarted, ContextLevel, ContextPolicy, ContextReduced, ContextState, Diagnostic,
  DiagnosticLevel, EpochReason, EventEnvelope, EventMeta, EventSeq, EventSink, ExternalContextItem,
  ExternalContextRetrieved, FailurePhase, Message, ModelCapabilities, ModelEpoch,
  ModelEpochStarted, ModelFailover, ModelFailure, ModelFailureKind, ModelProvider, ModelRef,
  ModelRequest, ModelRequestCompleted, ModelRequestStarted, ModelRetry, ReasoningDelta,
  ReasoningProvenance, ReductionReason, Role, SessionEndReason, SessionEnded, SessionId,
  SessionStarted, SinkError, ThinkingLevel, ToolCallBlock, ToolCompleted, ToolExecutionState,
  ToolFailed, ToolProgress, ToolRequested, ToolResultBlock, ToolStarted, ToolUnknown, TraceId,
  TurnCompleted, TurnId, TurnStatus, UserMessage,
};
use pi_rs_tools::{Executed, ToolRegistry};

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
  pub interrupted_tools: Vec<pi_rs_core::InterruptedToolCall>,
}

/// How many model round-trips one user input may take.
///
/// A model that keeps asking for tools is looping; the limit exists so a loop
/// costs one visible failure instead of an unbounded bill.
pub const MAX_MODEL_REQUESTS_PER_TURN: usize = 32;

/// Live feedback for the surface. Every method is optional by design: the loop
/// must be runnable with nobody watching.
pub trait TurnProgress: Send {
  fn on_user_message(&mut self, _text: &str) {}
  fn on_request_started(&mut self, _model: &ModelRef) {}
  fn on_reasoning(&mut self, _text: &str, _provenance: ReasoningProvenance) {}
  fn on_text_delta(&mut self, _text: &str) {}
  fn on_tool_requested(&mut self, _call: &ToolCallBlock) {}
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

  /// Persist one semantic message against the event that introduced it.
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
      Self::Sink(_) => None,
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
  introduced_by: EventEnvelope,
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
  /// Model requests spent by the current turn, retries and takeovers included.
  ///
  /// Counted where requests are issued rather than where rounds are driven, so the
  /// recovery loop cannot outlive the budget it is supposed to respect.
  requests: AtomicUsize,
  session_started: bool,
  /// Whether the first lifecycle event belongs to a continuation of an existing
  /// durable journal rather than a newly created session.
  resumed: bool,
  /// Interrupted tool lifecycles recovered from the canonical trace.
  interrupted_tools: Vec<pi_rs_core::InterruptedToolCall>,
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
  /// Last provider-reported input tokens, preferred over any estimate.
  measured_input_tokens: Option<u64>,
  /// Envelopes this loop produced. Journal positions come from the trace, and
  /// compaction needs them: the epoch names the canonical range it replaces,
  /// and a loop that cannot cite positions cites none.
  envelopes: Vec<EventEnvelope>,
  /// Journal bounds that predate this loop — a resumed session's earlier
  /// events — so a compaction after resume still names the full range it
  /// replaces. Stored as bounds, not the log itself: the loop cites history, it
  /// never replays it.
  history: Option<(pi_rs_core::EventSeq, pi_rs_core::EventSeq)>,
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
      requests: AtomicUsize::new(0),
      session_started: false,
      resumed: false,
      interrupted_tools: Vec::new(),
      recovery_blocked: false,
      context_epoch: 0,
      checkpoint_floor: 0,
      checkpoint_cited_from: None,
      last_compaction: None,
      measured_input_tokens: None,
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
    self.max_requests = max.max(1);
    self
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
      return Err(TurnError::Sink("no backup model configured".to_string()));
    };
    if self.active_model() == *backup.model() {
      return Err(TurnError::Sink(format!(
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
      return Err(TurnError::Sink(format!(
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
      return Err(TurnError::Sink(format!(
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
    request: &pi_rs_core::ToolRequest,
  ) -> Result<pi_rs_core::ReconciliationStatus, pi_rs_core::ToolError> {
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
        pi_rs_core::ReconciliationStatus::Committed { details } => (
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
        pi_rs_core::ReconciliationStatus::Unmodified { details } => (
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
        pi_rs_core::ReconciliationStatus::Diverged { details }
        | pi_rs_core::ReconciliationStatus::RequiresManualInspection { details } => {
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
      let envelope = match self.emit_message(Some(turn_id), event, &message) {
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
    // Recovery may rewrite only the history that predates this turn. Keep the
    // boundary local so one turn's emergency state cannot leak into the next.
    let mut turn_history_start = self.messages.len();
    let mut overflow_recovery_used = false;

    self.ensure_session_started()?;
    self.reconcile_interrupted_tools()?;

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
    // caller's side they cost the same.
    while self.requests.load(Ordering::SeqCst) < self.max_requests {
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
      report.epoch = response.epoch;
      report.requests = self.requests.load(Ordering::SeqCst);

      let mut blocks = Vec::new();
      if let Some(text) = response.text.as_ref().filter(|text| !text.is_empty()) {
        blocks.push(ContentBlock::text(text.clone()));
        report.text.push_str(text);
      }
      for call in &response.calls {
        blocks.push(ContentBlock::ToolCall(ToolCallBlock {
          id: call.id.clone(),
          name: call.name.clone(),
          arguments: call.arguments.clone(),
        }));
      }
      if !blocks.is_empty() {
        let message = Message::new(Role::Assistant, blocks);
        let introduced_by = response.introduced_by.clone();
        self.trace.record_message(&AttributedMessage {
          envelope: introduced_by.clone(),
          message: message.clone(),
        })?;
        self.push_message(message, introduced_by.meta.seq);
      } else {
        self
          .trace
          .complete_without_message(&response.introduced_by)?;
      }

      if response.calls.is_empty() {
        // The model answered instead of asking: the turn is over.
        return self.finish(report, TurnStatus::Completed, clock, Some(turn_id.clone()));
      }

      let tool_calls = u32::try_from(response.calls.len())
        .map_err(|_| TurnError::Sink("tool-call count exceeds durable limit".into()))?;
      report.tool_calls = report
        .tool_calls
        .checked_add(tool_calls)
        .ok_or_else(|| TurnError::Sink("turn tool-call count is exhausted".into()))?;
      self.execute_calls(turn_id.clone(), &response.calls, cancel, progress)?;
    }

    // Out of requests, not out of options: the distinction belongs in the trace.
    report.budget_exhausted = true;
    self.diagnostic(
      Some(turn_id.clone()),
      DiagnosticLevel::Warn,
      format!(
        "turn stopped after {} model requests without a final answer",
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

  fn emit_without_message(
    &mut self,
    turn_id: Option<TurnId>,
    event: AgentEvent,
  ) -> Result<EventEnvelope, TurnError> {
    self.emit_with_sink(turn_id, event, true)
  }

  fn emit_with_sink(
    &mut self,
    turn_id: Option<TurnId>,
    event: AgentEvent,
    without_message: bool,
  ) -> Result<EventEnvelope, TurnError> {
    let epoch = &self.epochs[self.epochs.len() - 1];
    let mut meta = EventMeta::new(self.session_id.clone(), self.trace_id.clone());
    meta.model_epoch = Some(epoch.index);
    meta.model = Some(epoch.model.clone());
    if let Some(turn_id) = turn_id {
      meta.turn_id = Some(turn_id);
    }
    let mut envelope = EventEnvelope::new(meta, event);
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
  fn first_cited_seq(&self) -> pi_rs_core::EventSeq {
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
              pi_rs_core::EventSeq(0)
            }
          })
      })
      .unwrap_or(pi_rs_core::EventSeq(0))
  }

  /// Declare the journal bounds of history this loop inherited but did not emit.
  ///
  /// A resumed loop is handed messages, not envelopes; without this, a
  /// compaction after resume would name a range that silently omits everything
  /// the previous run wrote.
  pub fn with_cited_history(
    mut self,
    first: pi_rs_core::EventSeq,
    last: pi_rs_core::EventSeq,
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
    let envelope = self.emit(turn_id, event)?;
    self.trace.record_message(&AttributedMessage {
      envelope: envelope.clone(),
      message: message.clone(),
    })?;
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
      progress.on_request_started(&model);

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
        committed,
        reasoning_provenance: provenance,
        assistant_introduced_by,
        sink_error,
        first_delta_ms,
        ..
      } = collector;
      if let Some(error) = sink_error {
        return Err(TurnFailure::Sink(error));
      }

      let failure = match outcome {
        // Transport said done. Whether the *model* finished is a separate
        // question, and answering it wrongly is how a runtime accepts a half
        // answer as final.
        Ok(usage) => {
          // "Produced" means *content the user would see*. Tool calls alone do not
          // make an unfinished response a truncation of an answer.
          let produced = !text.is_empty();
          match completion_failure(&usage, produced, committed) {
            Some(failure) => failure,
            None => {
              self.measured_input_tokens = usage.input_tokens;
              let tool_calls = u32::try_from(calls.len()).map_err(|_| {
                TurnFailure::Sink(SinkError("tool-call count exceeds durable limit".into()))
              })?;
              let introduced_by = self
                .emit(
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
                    output_tokens: usage.output_tokens,
                    duration_ms,
                    tool_calls,
                    reasoning_provenance: provenance,
                    first_delta_ms,
                  }),
                )
                .map_err(TurnFailure::from)?;
              return Ok(Response {
                epoch,
                text: (!text.is_empty()).then_some(text),
                calls,
                introduced_by: assistant_introduced_by.unwrap_or(introduced_by),
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
      self
        .emit(
          Some(turn_id.clone()),
          AgentEvent::ModelRequestCompleted(ModelRequestCompleted {
            epoch,
            model: model.clone(),
            finish_reason: None,
            input_tokens: None,
            output_tokens: None,
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
          self.rebudget(turn_id.clone(), turn_history_start)?
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
    turn_id: TurnId,
    turn_history_start: &mut usize,
  ) -> Result<u32, TurnError> {
    let target = self
      .failover
      .backup_capabilities
      .as_ref()
      .map(|caps| caps.context_window.saturating_sub(1_024))
      .unwrap_or(4_096);
    let checkpoint = self.checkpoint_floor.min(self.messages.len());
    let turn_start = (*turn_history_start).min(self.messages.len());
    let had_evictable_history = turn_start.max(checkpoint) > checkpoint;
    let dropped = self.evict_oldest(target, &turn_id, turn_history_start)?;
    // `safe_eviction_boundary` may refuse every candidate when the oldest
    // retained unit is an incomplete tool lifecycle. Do not switch epochs and
    // send a request that is known to exceed the backup budget in that case;
    // a smaller model cannot repair an invalid history by receiving it.
    if had_evictable_history && estimate_messages(&self.messages) > target {
      return Err(TurnError::Sink(format!(
        "cannot safely rebudget history below the backup context target of {target} tokens"
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
    self.normalize_message_seqs();
    let before = estimate_messages(&self.messages);
    let turn_start = (*turn_history_start).min(self.messages.len());
    // The current-turn suffix and a restored checkpoint capsule are both
    // non-evictable. The latter is a durable barrier, not ordinary history.
    let checkpoint = self.checkpoint_floor.min(self.messages.len());
    let evictable_start = checkpoint;
    let evictable_end = turn_start.max(checkpoint).min(self.messages.len());
    let max_drop = evictable_end.saturating_sub(evictable_start);
    let mut desired = 0usize;
    while desired < max_drop
      && estimate_after_eviction(&self.messages, evictable_start, desired) > target
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
      let visible = estimate_after_eviction(&self.messages, evictable_start, dropped);
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
    let mut objective = String::from("Perform assigned task");
    let mut completed_work = Vec::new();
    let mut artifacts = Vec::new();
    let mut decisions = Vec::new();
    let mut constraints = Vec::new();
    let unresolved = Vec::new();
    let next_actions = vec!["Continue session from checkpoint".to_string()];

    for msg in &self.messages {
      if msg.role == Role::User && !msg.text().trim().is_empty() {
        let text = msg.text();
        let first_line = text.lines().next().unwrap_or(&text).trim();
        if !first_line.is_empty() && !first_line.starts_with('[') {
          objective = first_line.chars().take(120).collect();
          break;
        }
      }
    }

    for msg in &self.messages {
      if msg.role == Role::Assistant {
        for block in &msg.content {
          match block {
            ContentBlock::ToolCall(call) => {
              let note = format!("Executed tool `{}`", call.name);
              if !completed_work.contains(&note) {
                completed_work.push(note);
              }
              if let Some(path_val) = call.arguments.get("path").and_then(|p| p.as_str()) {
                let path = path_val.to_string();
                if !artifacts.iter().any(|a: &CapsuleArtifact| a.path == path) {
                  artifacts.push(CapsuleArtifact {
                    path,
                    note: format!("referenced by `{}`", call.name),
                  });
                }
              }
            }
            ContentBlock::Text { text } => {
              let trimmed = text.trim();
              if trimmed.starts_with("Decision:") {
                decisions.push(CapsuleDecision {
                  decision: trimmed.chars().take(80).collect(),
                  rationale: "recorded during turn".into(),
                });
              }
            }
            _ => {}
          }
        }
      }
    }

    if let Some(sys) = &self.system {
      if sys.contains("constraint") || sys.contains("must") {
        constraints.push("Follow system prompt instructions".into());
      }
    }

    let current_state = format!(
      "Context pressure ({} tokens) in epoch {}; reason: {reason}",
      state.effective_tokens(),
      state.context_epoch
    );

    ContextCapsule {
      version: CAPSULE_SCHEMA_VERSION,
      objective,
      completed_work,
      decisions,
      constraints,
      current_state,
      artifacts,
      unresolved,
      next_actions,
    }
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

  /// Assemble the exact provider request shape without consulting or mutating
  /// context policy. Emergency overflow recovery uses this same constructor.
  fn assemble_request(&self, messages: Vec<Message>) -> ModelRequest {
    let capabilities = self.provider().capabilities();
    let tools = if capabilities.tools {
      self.tools.specs()
    } else {
      Vec::new()
    };
    let mut request = ModelRequest::new(self.active_model(), capabilities, messages)
      .with_tools(tools)
      .with_thinking(self.thinking);
    if let Some(system) = self.system.clone() {
      request = request.with_system(system);
    }
    request
  }

  /// Build the model request, consulting the context policy first.
  fn build_request(
    &mut self,
    turn_id: &TurnId,
    turn_history_start: &mut usize,
  ) -> Result<ModelRequest, TurnError> {
    let capabilities = self.provider().capabilities();
    let state = {
      let mut state = ContextState::zero(capabilities.context_window);
      state.context_epoch = self.context_epoch;
      state.measured_tokens = self.measured_input_tokens;
      state.estimated_tokens = estimate_messages(&self.messages);
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
      ContextAction::Warn { .. } | ContextAction::Keep | ContextAction::ReducePayload { .. } => {}
    }

    Ok(self.assemble_request(self.messages.clone()))
  }

  /// Close fully decoded calls from a response that cannot be acted on.
  fn record_unexecuted_calls(
    &mut self,
    turn_id: TurnId,
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
      self.emit(
        Some(turn_id.clone()),
        AgentEvent::ToolRequested(ToolRequested {
          call_id: call.id.clone(),
          name: call.name.clone(),
          arguments: call.arguments.clone(),
          read_only,
        }),
      )?;
      self.emit_without_message(
        Some(turn_id.clone()),
        AgentEvent::ToolFailed(ToolFailed {
          call_id: call.id.clone(),
          name: call.name.clone(),
          message: reason.to_string(),
          duration_ms: 0,
          status: None,
        }),
      )?;
    }
    Ok(())
  }

  /// Execute the calls one completed request asked for, in declaration order.
  fn execute_calls(
    &mut self,
    turn_id: TurnId,
    calls: &[ToolCallBlock],
    cancel: &CancelToken,
    progress: &mut dyn TurnProgress,
  ) -> Result<(), TurnError> {
    for call in calls {
      let metadata = self.tools.metadata_for(&call.name);
      let read_only = metadata
        .as_ref()
        .map(|meta| meta.read_only)
        .unwrap_or(false);
      progress.on_tool_requested(call);
      self.emit(
        Some(turn_id.clone()),
        AgentEvent::ToolRequested(ToolRequested {
          call_id: call.id.clone(),
          name: call.name.clone(),
          arguments: call.arguments.clone(),
          read_only,
        }),
      )?;

      if cancel.is_cancelled() {
        // A call the model asked for but that never ran is still recorded: it is
        // part of what the model decided.
        let block = ToolResultBlock {
          id: call.id.clone(),
          name: call.name.clone(),
          state: ToolExecutionState::Requested,
          text: "not executed: the turn was cancelled".to_string(),
          is_error: true,
          reduced: false,
        };
        let message = Message::new(Role::Tool, vec![ContentBlock::ToolResult(block)]);
        let envelope = self.emit_message(
          Some(turn_id.clone()),
          AgentEvent::ToolFailed(ToolFailed {
            call_id: call.id.clone(),
            name: call.name.clone(),
            message: "cancelled before execution".to_string(),
            duration_ms: 0,
            status: None,
          }),
          &message,
        )?;
        self.push_message(message, envelope.meta.seq);
        break;
      }

      let request = pi_rs_core::ToolRequest {
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
      let executed = {
        let mut sink = LiveToolSink { progress, call };
        let trace = &mut *self.trace;
        let mut on_started = || {
          let mut meta =
            EventMeta::new(attribution.session_id.clone(), attribution.trace_id.clone());
          meta.turn_id = Some(attribution.turn_id.clone());
          meta.model_epoch = Some(attribution.epoch);
          meta.model = Some(attribution.model.clone());
          let mut envelope = EventEnvelope::new(
            meta,
            AgentEvent::ToolStarted(ToolStarted {
              call_id: call.id.clone(),
              name: call.name.clone(),
            }),
          );
          trace.emit(&mut envelope)
        };
        let clock = Instant::now();
        let executed = self
          .tools
          .execute_observed(&request, &mut sink, cancel, &mut on_started)?;
        (executed, elapsed_ms(clock))
      };
      let (block, seq) =
        self.record_tool_outcome(turn_id.clone(), call, &executed.0, executed.1, read_only)?;
      progress.on_tool_finished(call, &executed.0);
      self.push_message(
        Message::new(Role::Tool, vec![ContentBlock::ToolResult(block)]),
        seq,
      );
    }
    Ok(())
  }

  /// Turn one executed call into its session block and its terminal event.
  fn record_tool_outcome(
    &mut self,
    turn_id: TurnId,
    call: &ToolCallBlock,
    executed: &Executed,
    duration_ms: u64,
    read_only: bool,
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
    let envelope = self.emit_message(
      Some(turn_id.clone()),
      event,
      &Message::new(Role::Tool, vec![ContentBlock::ToolResult(block.clone())]),
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
  committed: bool,
  reasoning_index: u32,
  text_index: u32,
  reasoning_provenance: Option<ReasoningProvenance>,
  assistant_introduced_by: Option<EventEnvelope>,
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
      committed: false,
      reasoning_index: 0,
      text_index: 0,
      reasoning_provenance: None,
      assistant_introduced_by: None,
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

impl pi_rs_core::ProviderEventSink for Collector<'_> {
  fn emit(&mut self, event: &pi_rs_core::ProviderEvent) {
    self.mark_first_delta();
    match event {
      pi_rs_core::ProviderEvent::ReasoningDelta { text, provenance } => {
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
      pi_rs_core::ProviderEvent::TextDelta(text) => {
        let traced = self.trace_event(AgentEvent::AssistantDelta(AssistantDelta {
          text: text.clone(),
          chunk_index: self.text_index,
        }));
        if let Some(envelope) = traced {
          self.text_index = self.text_index.saturating_add(1);
          self.committed = true;
          self.text.push_str(text);
          if self.assistant_introduced_by.is_none() {
            self.assistant_introduced_by = Some(envelope);
          }
          self.progress.on_text_delta(text);
        }
      }
      pi_rs_core::ProviderEvent::ToolCall(call) => {
        if self.sink_error.is_none() {
          self.committed = true;
          self.calls.push(call.clone());
        }
      }
    }
  }
}

/// A failure when the response was not a completion, `None` when it was.
fn completion_failure(
  usage: &pi_rs_core::CompletionUsage,
  produced: bool,
  committed: bool,
) -> Option<ModelFailure> {
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
  fn emit(&mut self, chunk: &pi_rs_core::ToolChunk) {
    self.progress.on_tool_progress(self.call, &chunk.text);
  }
}

fn elapsed_ms(clock: Instant) -> u64 {
  clock.elapsed().as_millis() as u64
}

/// Rough token estimate for one request, used only until a measurement exists.
///
/// The estimate deliberately includes the complete tool schema because a request
/// can fit by message bytes alone while still exceeding the provider window once
/// exposed tools are serialized.
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

/// Leave explicit headroom after a provider has proved the advertised window
/// estimate was optimistic. The same assembled request is measured before this
/// target is accepted, so system text, tools, and message ordering all count.
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

fn estimate_after_eviction(messages: &[Message], start: usize, dropped: usize) -> u64 {
  let end = start.saturating_add(dropped).min(messages.len());
  let bytes: usize = messages[..start]
    .iter()
    .chain(messages[end..].iter())
    .map(estimate_message_bytes)
    .sum();
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

/// Synthesize a structured factual summary of older conversation messages.
pub fn structured_summary(messages: &[Message]) -> String {
  let mut summary = String::from("Summary of earlier conversation:\n");
  for msg in messages {
    let text = msg.text();
    let trimmed = text.trim();
    match msg.role {
      Role::User if !trimmed.is_empty() => {
        summary.push_str("- User: ");
        let preview: String = trimmed
          .lines()
          .next()
          .unwrap_or(trimmed)
          .chars()
          .take(120)
          .collect();
        summary.push_str(&preview);
        summary.push('\n');
      }
      Role::Assistant => {
        for block in &msg.content {
          match block {
            ContentBlock::Text { text } => {
              let t_trimmed = text.trim();
              if !t_trimmed.is_empty() {
                summary.push_str("- Assistant: ");
                let preview: String = t_trimmed
                  .lines()
                  .next()
                  .unwrap_or(t_trimmed)
                  .chars()
                  .take(120)
                  .collect();
                summary.push_str(&preview);
                summary.push('\n');
              }
            }
            ContentBlock::ToolCall(call) => {
              summary.push_str(&format!("- Action: called tool `{}`\n", call.name));
            }
            _ => {}
          }
        }
      }
      Role::Tool => {
        summary.push_str("- Tool: completed execution\n");
      }
      _ => {}
    }
  }
  summary
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::{Arc, Mutex};

  use crate::StoreTrace;
  use pi_rs_core::Tool;
  use pi_rs_core::{
    CompletionUsage, ProviderEvent, ProviderEventSink, SessionHeader, ThinkingLevel, ToolChunk,
    ToolMetadata, ToolOutcome, ToolPolicy, ToolRequest,
  };
  use pi_rs_tools::Workspace;

  /// One recorded event: its turn, its wire kind, and its payload.
  type Recorded = (Option<TurnId>, String, serde_json::Value);

  /// Events as they were emitted. Sequence numbers are stamped like a durable
  /// log so tests can assert on journal coordinates, not only on kinds.
  #[derive(Clone, Default)]
  struct Recorder(Arc<Mutex<Vec<Recorded>>>);

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
      envelope.meta.seq = Some(pi_rs_core::EventSeq(recorded.len() as u64 + 1));
      recorded.push((envelope.meta.turn_id.clone(), kind, payload));
      Ok(())
    }

    fn put_payload(&mut self, _bytes: &[u8]) -> Result<Option<BlobRef>, SinkError> {
      // No blob store in these tests: reduction must still be reported, which is
      // what the `None` path exercises.
      Ok(None)
    }
  }

  impl Recorder {
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
          exposed_reasoning: pi_rs_core::ReasoningExposure::None,
          context_window: 128_000,
          max_output_tokens: Some(8_192),
        },
        rounds,
        fail: Vec::new(),
        calls: Arc::new(Mutex::new(Vec::new())),
        unfinished: false,
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
            ProviderEvent::ToolCall(_) => usage.finish_reason = Some("tool_calls".into()),
            ProviderEvent::ReasoningDelta { .. } => {}
          }
          sink.emit(event);
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
      id: pi_rs_core::ToolCallId::new(),
      name: name.into(),
      arguments,
    })]
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
      progress: &mut dyn pi_rs_core::ToolProgress,
    ) -> Result<ToolOutcome, pi_rs_core::ToolError> {
      progress.emit(&ToolChunk::new("working"));
      self.0.lock().unwrap().push(request.arguments.clone());
      Ok(ToolOutcome::succeeded("noted"))
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
      _progress: &mut dyn pi_rs_core::ToolProgress,
    ) -> Result<ToolOutcome, pi_rs_core::ToolError> {
      Ok(ToolOutcome::succeeded("ok"))
    }
  }

  /// A registry over a throwaway workspace, with `tools` auto-approved.
  fn registry_with(tools: Vec<Box<dyn Tool>>) -> ToolRegistry {
    let policy = pi_rs_core::ToolPolicy {
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
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
    let temp = pi_rs_store::TempDir::new("runtime-empty-completion");
    let store = pi_rs_store::Store::open(temp.path(), pi_rs_store::WritePolicy::default())
      .expect("store opens");
    let session_id = SessionId::new();
    let provider = Scripted::new("empty", vec![Vec::new()]);
    let tools = registry_with(Vec::new());
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let session = store
      .begin(SessionHeader {
        session_id: session_id.clone(),
        version: pi_rs_core::session::SESSION_SCHEMA_VERSION,
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
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
    assert_eq!(requests[0].system.as_deref(), Some("be brief"));
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
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let mut trace = Recorder::default();
    let source = pi_rs_core::trace::ExternalContextSource {
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
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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

    assert_eq!(request.system.as_deref(), Some("system instructions"));
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
          provenance: pi_rs_core::ReasoningProvenance::Native,
        },
        ProviderEvent::TextDelta("answer".into()),
      ]],
    );
    let tools = registry_with(Vec::new());
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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

    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
      .find(|message| message.role == pi_rs_core::Role::Tool)
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

    // And in order, which is what a replay needs to reconstruct the call honestly.
    let kinds = trace.kinds();
    let at = |wanted: &str| kinds.iter().position(|k| k == wanted).unwrap();
    assert!(
      at("tool_requested") < at("tool_started") && at("tool_started") < at("tool_completed"),
      "{kinds:?}"
    );
  }

  #[test]
  fn reduced_builtin_output_is_archived_and_redacted() {
    let temp = pi_rs_store::TempDir::new("runtime-reduced-read");
    let secret = "recovery-secret-91f7";
    let body: String = (1..=20_000)
      .map(|line| format!("{secret} line {line}\n"))
      .collect();
    std::fs::write(temp.child("large.txt"), body).unwrap();

    let mut write_policy = pi_rs_store::WritePolicy::default();
    write_policy.redaction.scan_environment = false;
    write_policy.redaction.literals = vec![secret.into()];
    let store = pi_rs_store::Store::open(temp.path(), write_policy).unwrap();
    let session_id = SessionId::new();
    let model = ModelRef::new("test", "read");
    let session = store
      .begin(SessionHeader {
        session_id: session_id.clone(),
        version: pi_rs_core::session::SESSION_SCHEMA_VERSION,
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
    let context = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
    let entries = pi_rs_store::TraceJournal::read(&trace_path).unwrap().items;
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
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
    let workspace_dir = pi_rs_store::TempDir::new("runtime-workspace");
    let outside = pi_rs_store::TempDir::new("runtime-outside");
    let outside_file = outside.child("outside.txt");
    std::fs::write(&outside_file, "outside\n").unwrap();
    let outside_file = outside_file.display().to_string();
    let outside_dir = outside.path().display().to_string();
    let call = |id: &str, name: &str, arguments: serde_json::Value| {
      ProviderEvent::ToolCall(ToolCallBlock {
        id: pi_rs_core::ToolCallId::from_string(id),
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
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
            id: pi_rs_core::ToolCallId::from_string("unknown-call"),
            name: "missing".into(),
            arguments: serde_json::json!({}),
          }),
          ProviderEvent::ToolCall(ToolCallBlock {
            id: pi_rs_core::ToolCallId::from_string("invalid-call"),
            name: "spy".into(),
            arguments: serde_json::json!("not an object"),
          }),
        ],
        text("done"),
      ],
    );
    let mut trace = Recorder::default();
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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

    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
    backup.capabilities.context_window = 1_024;
    let tools = registry_with(Vec::new());
    let mut trace = Recorder::default();
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
        id: pi_rs_core::ToolCallId::new(),
        name: "write".into(),
        arguments: serde_json::json!({"path":"state"}),
      }));
    let tools = registry_with(Vec::new());
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
      .rebudget(TurnId::new(), &mut turn_start)
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
  fn a_narrower_backup_drops_older_turns_before_it_takes_over() {
    // The other side of the same line: with real history in flight, takeover into a
    // small window shortens it, says so, and records what was given up.
    let primary =
      Scripted::new("primary", Vec::new()).always_fails(ModelFailureKind::ProviderUnavailable);
    let mut backup = Scripted::new("backup", vec![text("from a small window")]);
    // Target is the window less a kilotoken, so this backup can hold the newest turn
    // and little else.
    backup.capabilities.context_window = 1_100;
    let history = vec![
      Message::user("a".repeat(20_000)),
      Message::assistant("b".repeat(20_000)),
    ];
    let tools = registry_with(Vec::new());
    let mut trace = Recorder::default();
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
  fn an_unfinished_stream_is_not_reported_as_an_answer() {
    // The provider returned Ok with an uncertain boundary: the transport ended, the
    // model did not. Accepting that as a finished turn is how a harness silently
    // truncates answers.
    let provider = Scripted::uncertain("truncated", "half an an");
    let tools = registry_with(Vec::new());
    let mut trace = Recorder::default();

    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
  fn decoded_call_on_transport_failure_is_closed_without_execution() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let tools = registry_with(vec![Box::new(Spy(Arc::clone(&seen)))]);
    let provider = Scripted::new(
      "broken-after-call",
      vec![tool_call("spy", serde_json::json!({"value": 1}))],
    )
    .fails_after_stream(ModelFailureKind::Transport);
    let mut trace = Recorder::default();
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
    let temp = pi_rs_store::TempDir::new("runtime-uncertain-write");
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
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
  fn a_cancelled_turn_reports_cancelled_and_runs_no_tools() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let spy = Box::new(Spy(Arc::clone(&seen)));
    let tools = registry_with(vec![spy]);
    let provider = Scripted::new("caller", vec![tool_call("spy", serde_json::json!({}))]);
    let mut trace = Recorder::default();
    let cancel = CancelToken::new();
    cancel.cancel();

    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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

    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
    assert!(trace.kinds().iter().any(|kind| kind == "diagnostic"));
    let diagnostics = trace.0.lock().unwrap();
    assert!(diagnostics.iter().any(|(_, kind, payload)| {
      kind == "diagnostic"
        && payload["message"]
          .as_str()
          .unwrap_or_default()
          .contains("without a final answer")
    }));
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
      pi_rs_core::ProfilePolicy::new(pi_rs_core::ContextProfile::Balanced, PRESSURED_WINDOW);
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

    // 16 000 estimated tokens over a 5 000-token target drops the 11 oldest of
    // the 17 messages: the five newest old turns and the live prompt remain.
    let request = &provider.requests()[0];
    assert_eq!(request.messages.len(), 6, "evicted to the recent target");
    assert!(request.messages[0].text().starts_with('l'));
    assert_eq!(request.messages[5].text(), "go");

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
      pi_rs_core::ProfilePolicy::new(pi_rs_core::ContextProfile::Balanced, PRESSURED_WINDOW);
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
      pi_rs_core::ProfilePolicy::new(pi_rs_core::ContextProfile::Balanced, PRESSURED_WINDOW);
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
    let policy = pi_rs_core::ProfilePolicy::new(pi_rs_core::ContextProfile::Balanced, 32_768);
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
    let temp = pi_rs_store::TempDir::new("runtime-resume-prefix-checkpoint");
    let store = pi_rs_store::Store::open(temp.path(), pi_rs_store::WritePolicy::default())
      .expect("store opens");
    let session_id = SessionId::new();
    let model = ModelRef::new("test", "checkpoint-prefix");
    let session = store
      .begin(SessionHeader {
        session_id: session_id.clone(),
        version: pi_rs_core::session::SESSION_SCHEMA_VERSION,
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
    let policy = pi_rs_core::ProfilePolicy::new(pi_rs_core::ContextProfile::Balanced, 32_768);
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
    let temp = pi_rs_store::TempDir::new("runtime-resume-l0");
    let store = pi_rs_store::Store::open(temp.path(), pi_rs_store::WritePolicy::default())
      .expect("store opens");
    let session_id = SessionId::new();
    let model = ModelRef::new("test", "l0");
    let session = store
      .begin(SessionHeader {
        session_id: session_id.clone(),
        version: pi_rs_core::session::SESSION_SCHEMA_VERSION,
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
    let policy = pi_rs_core::ProfilePolicy::new(pi_rs_core::ContextProfile::Balanced, 32_768);
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
    let target = estimate_messages(&runtime.messages()[2..]);
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
    let temp = pi_rs_store::TempDir::new("runtime-resume-compaction");
    let store = pi_rs_store::Store::open(temp.path(), pi_rs_store::WritePolicy::default())
      .expect("store opens");
    let session_id = SessionId::new();
    let model = ModelRef::new("test", "resume");
    let session = store
      .begin(SessionHeader {
        session_id: session_id.clone(),
        version: pi_rs_core::session::SESSION_SCHEMA_VERSION,
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
    let policy = pi_rs_core::ProfilePolicy::new(pi_rs_core::ContextProfile::Balanced, 32_768);
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
    let temp = pi_rs_store::TempDir::new("runtime-resume-tool");
    let target = temp.child("already-written.txt");
    std::fs::write(&target, "committed").unwrap();
    let provider = Scripted::new("resume-tool", vec![text("continue")]);
    let tools = ToolRegistry::new(Workspace::new(temp.path()).unwrap())
      .with_policy(&ToolPolicy {
        auto_approve_mutating: true,
        ..ToolPolicy::default()
      })
      .with_builtins();
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
      provider.capabilities().context_window,
    );
    let old_turn = TurnId::new();
    let pending = pi_rs_core::InterruptedToolCall {
      request: pi_rs_core::ToolRequest {
        call_id: pi_rs_core::ToolCallId::new(),
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
    let temp = pi_rs_store::TempDir::new("runtime-resume-manual-tool");
    let tools = ToolRegistry::new(Workspace::new(temp.path()).unwrap())
      .with_policy(&ToolPolicy {
        auto_approve_mutating: true,
        ..ToolPolicy::default()
      })
      .with_builtins();
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
      interrupted_tools: vec![pi_rs_core::InterruptedToolCall {
        request: pi_rs_core::ToolRequest {
          call_id: pi_rs_core::ToolCallId::new(),
          name: "exec".into(),
          arguments: serde_json::json!({"command": "echo unsafe"}),
        },
        state: ToolExecutionState::Started,
        read_only: false,
        turn_id: Some(TurnId::new()),
        epoch: Some(0),
        model: Some(provider.model().clone()),
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
    let temp = pi_rs_store::TempDir::new("runtime-first-checkpoint");
    let store = pi_rs_store::Store::open(temp.path(), pi_rs_store::WritePolicy::default())
      .expect("store opens");
    let session_id = SessionId::new();
    let model = ModelRef::new("test", "checkpoint");
    let session = store
      .begin(SessionHeader {
        session_id: session_id.clone(),
        version: pi_rs_core::session::SESSION_SCHEMA_VERSION,
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
    let policy = pi_rs_core::ProfilePolicy::new(pi_rs_core::ContextProfile::Balanced, 32_768);
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
    let policy = pi_rs_core::ProfilePolicy::new(pi_rs_core::ContextProfile::Balanced, 32_768);
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
  }

  #[test]
  fn summary_compaction_with_only_a_checkpoint_tail_is_a_noop() {
    let provider = Scripted::new("checkpoint-only-tail", vec![text("ok")]);
    let tools = registry_with(Vec::new());
    let policy = pi_rs_core::ProfilePolicy::new(pi_rs_core::ContextProfile::Balanced, 32_768);
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
      pi_rs_core::ProfilePolicy::new(pi_rs_core::ContextProfile::Balanced, PRESSURED_WINDOW);
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
    let mut harness = harness.with_cited_history(pi_rs_core::EventSeq(1), pi_rs_core::EventSeq(2));
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
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
    let temp = pi_rs_store::TempDir::new("compaction-epoch");
    let store = pi_rs_store::Store::open(temp.path(), pi_rs_store::WritePolicy::default())
      .expect("store opens");
    let session_id = SessionId::new();
    let model = pi_rs_core::ModelRef::new("test", "durable");
    let session = store
      .begin(pi_rs_core::SessionHeader {
        session_id: session_id.clone(),
        version: pi_rs_core::session::SESSION_SCHEMA_VERSION,
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
    let policy = pi_rs_core::ProfilePolicy::new(
      pi_rs_core::ContextProfile::Balanced,
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
    let journal = pi_rs_store::TraceJournal::read(trace.session().trace_path())
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
    assert_eq!(epoch["replaces_through"], 5);
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
    assert_eq!(restored.compactions[0].replaces_through, Some(EventSeq(5)));
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
      pi_rs_core::ProfilePolicy::new(pi_rs_core::ContextProfile::Balanced, PRESSURED_WINDOW);
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
      pi_rs_core::ProfilePolicy::new(pi_rs_core::ContextProfile::Balanced, PRESSURED_WINDOW);
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
      pi_rs_core::ProfilePolicy::new(pi_rs_core::ContextProfile::Balanced, PRESSURED_WINDOW);
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
    let policy = pi_rs_core::ProfilePolicy::new(pi_rs_core::ContextProfile::Balanced, 64_000);
    let mut trace = Recorder::default();

    let external_source = pi_rs_core::trace::ExternalContextSource {
      provider: "rkb-rs".into(),
      resource_id: "doc-123".into(),
      provenance: "rkb-rs/citation".into(),
    };

    let item = ExternalContextItem::inline(
      external_source,
      "Key knowledge: pi-rs is written in Rust 2024.",
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
        && m.text().contains("pi-rs is written in Rust 2024")),
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
      pi_rs_core::ProfilePolicy::new(pi_rs_core::ContextProfile::Balanced, PRESSURED_WINDOW);
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
      pi_rs_core::ProfilePolicy::new(pi_rs_core::ContextProfile::Balanced, PRESSURED_WINDOW);
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
    let policy = pi_rs_core::ProfilePolicy::new(pi_rs_core::ContextProfile::Balanced, 32_768);
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
    let policy = pi_rs_core::ProfilePolicy::new(pi_rs_core::ContextProfile::Balanced, 32_768);
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
      cited_history: Some((pi_rs_core::EventSeq(1), pi_rs_core::EventSeq(17))),
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
    let policy = pi_rs_core::ProfilePolicy::new(pi_rs_core::ContextProfile::Balanced, 32_768);
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
    assert!(format!("{err:?}").contains("no backup model configured"));
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
    let policy = pi_rs_core::ProfilePolicy::new(pi_rs_core::ContextProfile::Balanced, 32_768);
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
    let policy = pi_rs_core::ProfilePolicy::new(pi_rs_core::ContextProfile::Balanced, 32_768);
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
    let policy = pi_rs_core::ProfilePolicy::new(pi_rs_core::ContextProfile::Balanced, 32_768);
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
