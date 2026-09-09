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

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use pi_rs_core::{
  AgentEvent, AssistantDelta, AttributedMessage, BlobRef, CancelToken, CapabilityGap, ContentBlock,
  ContextAction, ContextCompactionCompleted, ContextCompactionEpoch, ContextCompactionStarted,
  ContextLevel, ContextPolicy, ContextReduced, ContextState, Diagnostic, DiagnosticLevel,
  EpochReason, EventEnvelope, EventMeta, EventSink, FailurePhase, Message, ModelCapabilities,
  ModelEpochStarted, ModelFailover, ModelFailure, ModelFailureKind, ModelProvider, ModelRef,
  ModelRequest, ModelRequestCompleted, ModelRequestStarted, ModelRetry, ReasoningDelta,
  ReasoningProvenance, ReductionReason, Role, SessionEndReason, SessionEnded, SessionId,
  SessionStarted, SinkError, ThinkingLevel, ToolCallBlock, ToolCompleted, ToolExecutionState,
  ToolFailed, ToolProgress, ToolRequested, ToolResultBlock, ToolStarted, ToolUnknown, TraceId,
  TurnCompleted, TurnId, TurnStatus, UserMessage,
};
use pi_rs_tools::{Executed, ToolRegistry};

use crate::failover::{FailoverPolicy, Recovery};

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

  /// Push buffered events to their final destination.
  fn flush(&mut self) -> Result<(), SinkError> {
    Ok(())
  }
}

impl<T: Trace + ?Sized> Trace for &mut T {
  fn emit(&mut self, envelope: &mut EventEnvelope) -> Result<(), SinkError> {
    <T as Trace>::emit(self, envelope)
  }

  fn record_message(&mut self, attributed: &AttributedMessage) -> Result<(), SinkError> {
    <T as Trace>::record_message(self, attributed)
  }

  fn put_payload(&mut self, bytes: &[u8]) -> Result<Option<BlobRef>, SinkError> {
    <T as Trace>::put_payload(self, bytes)
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
  /// Out of requests. The budget is the user's, and maintenance spends it
  /// like an answer does: when the summarization a turn needed can no longer
  /// be asked, the honest report is exhaustion, not a model failure.
  Exhausted,
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
        TurnStatus::Completed | TurnStatus::Cancelled => None,
      },
      Self::Sink(_) | Self::Exhausted => None,
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
  context_epoch: u32,
  /// When this loop last shed model-visible history, for the policy's compaction
  /// cooldown. `None` until the first eviction; a loop that has never compacted
  /// has waited longer than any cooldown.
  last_compaction: Option<Instant>,
  /// A request has crossed this provider's window during this loop's life.
  ///
  /// The context policy treats this as an observation, not a guess: from here
  /// on it *requires* compaction at the safe boundary instead of suggesting
  /// one, because its own thresholds have already been wrong once.
  overflow_seen: bool,
  /// The *provider* reported the crossing, over content it had already
  /// streamed. The loop answers a runtime refusal by summarizing; a crossing
  /// reported on streamed content is reported back, never rewritten.
  provider_crossed: bool,
  /// The question this turn was asked with, kept for the maintenance that
  /// a refused request may need: the re-issue must carry it, and a
  /// summarization must not sweep it into its prefix.
  turn_question: String,
  /// The summarized candidate maintenance produced for the re-issue. The
  /// context check answers *it* rather than the full history: deciding
  /// twice, by two different questions, is how a loop asks a question it
  /// has already refused to answer.
  maintenance_candidate: Option<ModelRequest>,
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
      system: None,
      working_dir: String::new(),
      thinking: ThinkingLevel::default(),
      max_requests: MAX_MODEL_REQUESTS_PER_TURN,
      requests: AtomicUsize::new(0),
      session_started: false,
      context_epoch: 0,
      last_compaction: None,
      overflow_seen: false,
      provider_crossed: false,
      turn_question: String::new(),
      maintenance_candidate: None,
      measured_input_tokens: None,
      envelopes: Vec::new(),
      history: None,
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
    self.failover = policy;
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
    self.messages = messages;
    self
  }

  /// The model that owns generation right now.
  pub fn active_model(&self) -> ModelRef {
    self.epochs[self.epochs.len() - 1].model.clone()
  }

  /// `true` once a failover has occurred.
  pub fn failed_over(&self) -> bool {
    self.epochs.len() > 1
  }

  /// Model-visible history so far.
  /// Test-visible view of the live model context.
  pub fn messages_mut(&mut self) -> &mut Vec<Message> {
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
    let turn_id = TurnId::new();
    let clock = Instant::now();
    let mut report = TurnReport::new(turn_id.clone(), self.epoch_index());
    // One summarization per turn: the epoch it opens moves the estimate well
    // below the trigger, and a second model call in the same turn would be the
    // loop deciding twice that the user's history is too long.
    let mut summarized = false;

    self.ensure_session_started()?;
    // The size question has to be asked before the history grows the answer
    // into it: the user message lands on the trace below, and a session that
    // is already over the window with the question on it has one honest
    // refusal and no summarization that cannot help.
    self.turn_question = input.to_string();
    let user = Message::user(input);
    self.emit_message(
      Some(turn_id.clone()),
      AgentEvent::UserMessage(UserMessage {
        text: input.to_string(),
        attachments: 0,
      }),
      &user,
    )?;
    self.messages.push(user);
    progress.on_user_message(input);

    // The loop is bounded by *requests*, not rounds: a turn that keeps asking for
    // tools and a turn that keeps retrying spend the same budget, because from the
    // caller's side they cost the same.
    while self.requests.load(Ordering::SeqCst) < self.max_requests {
      if cancel.is_cancelled() {
        return self.finish(report, TurnStatus::Cancelled, clock, Some(turn_id.clone()));
      }
      // The build's arithmetic runs first and refuses before a byte is sent.
      // That refusal is the same event as the provider's own, and maintenance
      // answers it the same way: once, by summarizing, and only from state
      // that cannot have committed anything yet.


      // The request is built before anything streams, because that is the
      // only state in which a size refusal can be taken back: nothing has
      // committed, so a summarization can re-issue the same turn honestly.
      // A refusal *here* is the build's own arithmetic against the window,
      // and it goes to the same maintenance the provider's refusal gets —
      // the two differ only in who measured, and both are asked before the
      // history is spent.
      eprintln!("PREATTEMPT real={}", self.messages.len());
    let response = match self.attempt(turn_id.clone(), cancel, progress) {
        Ok(response) => response,
        // Cancellation is reported, never recovered from.
        Err(TurnFailure::Cancelled) => {
          return self.finish(report, TurnStatus::Cancelled, clock, Some(turn_id.clone()));
        }
        // A refusal of size — from the build's arithmetic or from the
        // provider's own mouth — is the one failure the loop answers
        // itself, once, by summarizing and re-issuing.
        Err(error @ TurnFailure::Aborted(TurnError::Aborted(_))) => {
          let error = match error {
            TurnFailure::Aborted(error) => error,
            other => return Err(turn_error_of(other)),
          };
          if self.maintenance_of(&error).is_none() || summarized {
            return Err(error);
          }
          let crossed = self.provider_crossed;
          match self.maintain(&error, &crossed, &turn_id, cancel, progress, &mut report, clock) {
            Ok(()) => continue,
            Err(end) => return Err(end),
          }
        }
        Err(TurnFailure::Aborted(error)) => return Err(error),
        Err(TurnFailure::Fatal(failure)) => {
          // The turn ends *before* the error is returned. A trace with no
          // `turn_completed` cannot tell a crashed session from an interrupted one,
          // and a caller that gets an error still needs the session to be coherent.
          report.status = TurnStatus::Failed { kind: failure.kind };
          let status = TurnStatus::Failed { kind: failure.kind };
          let _ = self.finish(report, status, clock, Some(turn_id.clone()))?;
          return Err(TurnError::Unavailable(failure));
        }
        Err(TurnFailure::Sink(error)) => return Err(TurnError::from(error)),
      };
      report.epoch = response.epoch;
      report.requests = self.requests.load(Ordering::SeqCst);

      let mut blocks = Vec::new();
      if let Some(text) = response.text.as_ref() {
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
        self.trace.record_message(&AttributedMessage {
          envelope: response.introduced_by.clone(),
          message: message.clone(),
        })?;
        self.messages.push(message);
      }

      if response.calls.is_empty() {
        // The model answered instead of asking: the turn is over.
        return self.finish(report, TurnStatus::Completed, clock, Some(turn_id.clone()));
      }

      report.tool_calls += response.calls.len() as u32;
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
    self.finish(report, TurnStatus::Completed, clock, Some(turn_id.clone()))
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
    let epoch = self.epochs[0].clone();
    self.emit(
      None,
      AgentEvent::SessionStarted(SessionStarted {
        working_dir: self.working_dir.clone(),
        model: epoch.model.clone(),
        capabilities: epoch.capabilities.clone(),
        resumed: false,
      }),
    )?;
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
    let epoch = &self.epochs[self.epochs.len() - 1];
    let mut meta = EventMeta::new(self.session_id.clone(), self.trace_id.clone());
    meta.model_epoch = Some(epoch.index);
    meta.model = Some(epoch.model.clone());
    if let Some(turn_id) = turn_id {
      meta.turn_id = Some(turn_id);
    }
    let mut envelope = EventEnvelope::new(meta, event);
    self.trace.emit(&mut envelope)?;
    self.envelopes.push(envelope.clone());
    Ok(envelope)
  }

  /// The oldest journal position this loop can cite.
  ///
  /// A compaction replaces the whole model-visible window: everything from the
  /// start of the journal, whether the records were written by this process or
  /// restored before it started. That is always position one of a durable
  /// session, and `EventSeq(0)` for a purely in-memory loop with no journal.
  fn first_cited_seq(&self) -> pi_rs_core::EventSeq {
    self
      .history
      .map(|(first, _)| first)
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

  /// The newest journal position this loop can cite, if any trace reported one.
  fn last_cited_seq(&self) -> pi_rs_core::EventSeq {
    self
      .envelopes
      .iter()
      .filter_map(|envelope| envelope.meta.seq)
      .max()
      .into_iter()
      .chain(self.history.map(|(_, last)| last))
      .max()
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

  /// One model request, with retries and at most one takeover.
  ///
  /// The loop may re-issue only while nothing has been streamed. That single rule
  /// is what keeps recovery from duplicating committed content.
  /// Whether a refusal is maintenance's to answer: a size refusal, stated
  /// either by the build's own arithmetic or by the provider before any of
  /// its content was committed. Anything else stands exactly as it was.
  fn maintenance_of(&self, error: &TurnError) -> Option<()> {
    matches!(
      error,
      TurnError::Aborted(TurnStatus::Failed {
        kind: ModelFailureKind::ContextOverflow,
      })
    )
    .then_some(())
  }

  /// The one maintenance a refused turn gets: summarize the oldest history
  /// the window can carry a re-issue beside, open the epoch that makes it
  /// durable, and hand the re-issued request to the next build. `Ok` means
  /// the turn was kept alive; `Err` is the turn's final error, and the
  /// refusal that ends a maintained turn is reported, not retried.
  fn maintain(
    &mut self,
    refusal: &TurnError,
    provider_crossed: &bool,
    turn_id: &TurnId,
    cancel: &CancelToken,
    progress: &mut dyn TurnProgress,
    report: &mut TurnReport,
    clock: Instant,
  ) -> Result<(), TurnError> {
    // The turn that is about to be lost is what tells the next one that
    // this window really does get crossed; recording it is not a policy,
    // it is the trace keeping its promise.
    self.overflow_seen = true;
    let window = self.provider().capabilities().context_window;
    let total = self.messages.len();
    eprintln!("MAINT real={total} texts={:?}", self.messages.iter().map(|m| m.text().len()).collect::<Vec<_>>());
    let tolerance = (window / 8).max(1);
    let capabilities = self.provider().capabilities();
    let model = self.active_model();
    let system = self.system.clone();
    let thinking = self.thinking;
    // Maintenance is decided of the request the re-issue will actually
    // carry: a summary stand-in for a prefix, the tail and the question
    // kept verbatim. The provider answers requests by their own size, so
    // this is the size that has to matter: it must fit the window that
    // refused the turn. Near the bound the estimate cannot measure; only a
    // re-issue worth asking for is worth its request. The empty prefix is
    // maintenance too: at the smallest windows the summary *is* the
    // history, and the instruction alone is the ask.
    let reissue_fits = |size: usize| {
      estimate_tokens(&reissue_of(&self.messages, &size, &system, &capabilities, &model, thinking))
        <= window
    };
    let worth_asking = |size: usize| {
      estimate_tokens(&reissue_of(
        &self.messages,
        &size,
        &system,
        &capabilities,
        &model,
        thinking,
      )) + tolerance
        >= window
    };
    // A request the provider itself refused is answered with the smallest
    // prefix that re-issues: keep every message the window can carry beside
    // the summary, because that is exactly the margin the provider just
    // proved exists. A refusal measured before the request left is answered
    // the other way, largest candidate first: maintenance earns its ask only
    // when it brings a request near the bound, and near the bound the
    // estimate — not the ask — decides how much history still fits beside
    // the summary.
    let candidate = if *provider_crossed {
      (1..=total).find(|size| reissue_fits(*size))
    } else {
      (1..=total)
        .rev()
        .find(|size| reissue_fits(*size) && worth_asking(*size))
    };
    eprintln!("SCAN window={window} total={total} crossed={provider_crossed} cand={candidate:?} fits={:?}",
      (1..=total).map(reissue_fits).collect::<Vec<_>>());
    eprintln!(
      "MAINT crossed={provider_crossed} window={window} total={total} candidate={candidate:?} fits[1]={} fits[{}]={}",
      reissue_fits(1),
      total,
      reissue_fits(total)
    );
    let Some(prefix) = candidate else {
      return Err(refusal.clone());
    };
    // A summarization and its re-issue are two requests: the budget that
    // ends at this turn pays for both, or maintenance is a summary nobody
    // can answer. Exhaustion is the honest end, and it is stated before
    // the epoch is opened, not after the trace says history was reduced
    // for nothing.
    let spare = self
      .max_requests
      .saturating_sub(self.requests.load(Ordering::SeqCst));
    if spare < 2 {
      self.diagnostic(
        Some(turn_id.clone()),
        DiagnosticLevel::Warn,
        "request budget cannot pay for a summarization and its re-issue",
      )?;
      report.budget_exhausted = true;
      let finished = self.finish(
        std::mem::replace(report, TurnReport::new(turn_id.clone(), report.epoch)),
        TurnStatus::Failed {
          kind: ModelFailureKind::ContextOverflow,
        },
        clock,
        Some(turn_id.clone()),
      )?;
      drop(finished);
      return Err(refusal.clone());
    }
    if !self.summarize_oldest(prefix, turn_id, turn_id, cancel, progress)? {
      // The provider refused to summarize: maintenance has no answer,
      // and the caller's refusal stands with the last word.
      return Err(refusal.clone());
    }
    // The history compaction actually wrote, not the scan's arithmetic:
    // maintenance drains the prefix it is given, so the request that can be
    // re-issued is the one measured against the history as it stands. A
    // summarization that left no smaller history behind is maintenance
    // answering its own question, and the refusal keeps the last word.
    let candidate = summarized_candidate(
      &self.messages,
      1,
      self.system.as_ref(),
      &self.provider().capabilities(),
      &self.active_model(),
      self.thinking,
    );
    if estimate_messages(&candidate.messages) <= window {
      self.maintenance_candidate = Some(candidate);
      return Ok(());
    }
    // No prefix left a request that fits: maintenance has spent its
    // request, and the same refusal answered twice is the turn's result.
    Err(refusal.clone())
  }

  fn attempt(
    &mut self,
    turn_id: TurnId,
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
      attempts_on_model += 1;
      let request = self.build_request(&turn_id).map_err(TurnFailure::from)?;
      eprintln!(
        "ROUND {} [{}]",
        estimate_messages(&request.messages),
        request.messages.iter().map(|m| m.text().len().to_string()).collect::<Vec<_>>().join(",")
      );

      // The turn's request budget is spent here, at the point the request exists.
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
            message_count: request.messages.len() as u32,
            context_tokens_est: estimate,
            tools_exposed: request.tools.len() as u32,
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
      let mut collector = Collector::new(progress, &mut *self.trace, attribution, cancel.clone());
      let outcome = provider.stream(&request, &mut collector, cancel);
      let duration_ms = elapsed_ms(clock);
      let Collector {
        text,
        calls,
        committed,
        reasoning_provenance: provenance,
        assistant_introduced_by,
        sink_error,
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
              let tool_calls = calls.len() as u32;
              let introduced_by = self
                .emit(
                  Some(turn_id.clone()),
                  AgentEvent::ModelRequestCompleted(ModelRequestCompleted {
                    epoch,
                    model: model.clone(),
                    finish_reason: usage.finish_reason.clone(),
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                    duration_ms,
                    tool_calls,
                    reasoning_provenance: provenance,
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
        Err(refusal) => {
          // A refusal for size is not a fault to recover from: the model was
          // healthy, it was the request that could not fit. Retrying sends the
          // same impossible request, and failing over credits a backup with
          // fixing a problem the history caused. This is the one place that
          // learns the window has actually been crossed, and the turn loop
          // answers it the only way a window permits: by summarizing.
          if refusal.kind == ModelFailureKind::ContextOverflow {
            self.overflow_seen = true;
            // The provider refused the request it was given: its window is
            // a fact about this history that the estimate did not reach.
            // A crossing over text the user has already seen is the one
            // crossing no summary may answer, and the caller reports it.
            self.provider_crossed = true;
            if committed {
              return Err(TurnFailure::Fatal(refusal));
            }
            // Otherwise the request was refused *before* anything was served*.
            // Retrying it sends the same impossible request to the same window,
            // and a failover would credit a backup with fixing what only
            // shorter history fixes, so the attempt ends here: the turn loop
            // answers a pre-content refusal the only way a window permits, by
            // summarizing once and re-issuing.
            return Err(TurnFailure::Aborted(TurnError::Aborted(
              TurnStatus::Failed {
                kind: ModelFailureKind::ContextOverflow,
              },
            )));
          }
          // Output reached the user the moment it was emitted, so it is recorded on
          // the failure rather than inferred afterwards.
          let mut failure = refusal;
          failure.partial_output_emitted = committed;
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
            tool_calls: calls.len() as u32,
            reasoning_provenance: provenance,
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
      match self
        .recover(turn_id.clone(), &failure, cancel)
        .map_err(TurnFailure::from)?
      {
        Action::Retry | Action::Takeover => continue,
        Action::Stop => return Err(TurnFailure::Fatal(failure)),
      }
    }
  }

  /// The provider for the active epoch.
  ///
  /// After a takeover only the backup may serve requests: the architecture says
  /// stay on the backup until the user says otherwise, so the primary is never
  /// silently re-activated mid-session.
  fn provider(&self) -> &'a dyn ModelProvider {
    if self.epochs.len() > 1 {
      self.backup.unwrap_or(self.primary)
    } else {
      self.primary
    }
  }

  /// Apply the failover policy's decision to a failure.
  fn recover(
    &mut self,
    turn_id: TurnId,
    failure: &ModelFailure,
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
        if narrow {
          self.rebudget(turn_id.clone())?;
        }
        // Whether the takeover request can actually be served: a backup whose
        // window cannot hold the history that survives rebudget shortens
        // nothing by taking over, and the epoch must say so.
        let fitted = estimate_messages(&self.messages)
          <= backup.capabilities().context_window.saturating_sub(1_024);
        let from = self.active_model();
        let epoch = Epoch {
          index: self.epoch_index() + 1,
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
            // Whether the request the backup now receives actually fits
            // its window — not merely whether turns were dropped or whether the
            // window is smaller. A reduction that never happened and a takeover
            // that cannot be served are both facts the epoch keeps, not
            // provenance this runtime invents.
            compacted: fitted,
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
  fn rebudget(&mut self, turn_id: TurnId) -> Result<u32, TurnError> {
    let target = self
      .failover
      .backup_capabilities
      .as_ref()
      .map(|caps| caps.context_window.saturating_sub(1_024))
      .unwrap_or(4_096);
    self.evict_oldest(target, &turn_id)
  }

  /// Drop the oldest model-visible turns until the estimate reaches `target`, and
  /// record the fact with a recovery reference.
  ///
  /// This is the runtime's own compaction tier: no model, no summary, nothing
  /// outside the model-visible vector. The canonical trace is untouched — it holds
  /// every dropped turn — and the event says what left the window and where the
  /// proof lives. The newest turn is never dropped: without it there is nothing to
  /// continue.
  fn evict_oldest(&mut self, target: u64, turn_id: &TurnId) -> Result<u32, TurnError> {
    let before = estimate_messages(&self.messages);
    let mut dropped = 0u32;
    while estimate_messages(&self.messages) > target && self.messages.len() > 1 {
      self.messages.remove(0);
      dropped += 1;
    }
    if dropped > 0 {
      self.context_epoch += 1;
      self.last_compaction = Some(Instant::now());
      let blob = self.trace.put_payload(
        format!("dropped {dropped} oldest turns to reach {target} tokens").as_bytes(),
      )?;
      let recovery_ref = blob.as_ref().map(BlobRef::recovery_ref);
      self.emit(
        Some(turn_id.clone()),
        AgentEvent::ContextReduced(ContextReduced {
          reason: ReductionReason::RecentTargetExceeded {
            target_tokens: target,
          },
          original_bytes: before,
          visible_bytes: estimate_messages(&self.messages),
          recovery_ref,
          blob,
          tool_call_id: None,
        }),
      )?;
    }
    Ok(dropped)
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
  /// - the live context becomes the retained tail plus the summary.
  ///
  /// Restoration does not yet consume epoch records: a resumed session replays
  /// the full log, which is correct but not compact. That consumption belongs
  /// with the reduction policy that will call this at window pressure.
  pub fn compact(
    &mut self,
    turn_id: &TurnId,
    summary: &str,
    retained: usize,
  ) -> Result<u32, TurnError> {
    self.compact_with(turn_id, summary, retained, None, None)
  }

  /// The compaction itself, with room for maintenance to hand it the
  /// summary's place in the history: a stand-in the re-issue was measured
  /// against becomes the summary, at the position it stands in.
  fn compact_with(
    &mut self,
    turn_id: &TurnId,
    summary: &str,
    retained: usize,
    summary_position: Option<usize>,
    stand_in: Option<&Message>,
  ) -> Result<u32, TurnError> {
    let replaces_tail = summary_position.is_some();
    // `retained` counts the messages that survive *besides* the summary, which
    // is added on top. An empty context keeps nothing; any other context always
    // keeps its newest message, so a compaction always replaces a range.
    let kept = if self.messages.is_empty() {
      0
    } else {
      // One slot is reserved for the summary itself, so `retained` counts only
      // the messages that survive *besides* it. It may be zero: then only the
      // summary stands, and a one-message context still loses its only message.
      retained.min(self.messages.len().saturating_sub(1))
    };
    let removed = self.messages.len() - kept;
    if removed == 0 {
      // Nothing to compact: summarizing a single-message context would open an
      // epoch that replaced no range.
      return Ok(0);
    }
    // Positions must be read before anything new is appended: the replaced
    // range ends at the newest record that existed when compaction began.
    let replaces_from = self.first_cited_seq();
    let replaces_through = self.last_cited_seq();
    self.context_epoch += 1;
    self.emit(
      Some(turn_id.clone()),
      AgentEvent::ContextCompactionStarted(ContextCompactionStarted {
        level: ContextLevel::L1Ordinary,
        reason: format!("summarizing {removed} oldest messages"),
      }),
    )?;

    // Canonical first: the summary is durable before the live context forgets
    // the messages it replaced. It enters history as a user message: that is the
    // role providers accept mid-context, and the text itself says it is a summary.
    //
    // A summary that stands *ahead of the retained tail* is what maintenance
    // builds its re-issue on, and it is the one shape a window that just
    // refused a request can be trusted with: its size is measured against the
    // window before the ask is sent, and a stand-in of the summary's own
    // reserve is what that measurement walks. A caller that has already put
    // such a stand-in in the history passes it here, and the summary that
    // answers takes its place where it stands.
    let summary_message = if replaces_tail {
      match stand_in {
        // The stand-in is where the re-issue was measured, and a summary
        // that outgrew it would cross the window the scan just measured.
        // The text is kept to the stand-in's own bytes, cut whole at the
        // last word boundary; what does not fit was never in the request
        // the scan trusted anyway.
        Some(stand_in) => {
          let text = format!("{summary}\n{SUMMARY_MARK}");
          let budget = stand_in.text().len().max(1);
          let kept = match text[..budget.min(text.len())].rfind(' ') {
            Some(edge) if edge > budget / 2 => &text[..edge],
            _ => text.as_str(),
          };
          let mut message = stand_in.clone();
          message.content = vec![pi_rs_core::ContentBlock::text(kept)];
          message
        }
        None => Message::system(format!("{summary}\n{SUMMARY_MARK}")),
      }
    } else {
      Message::user(summary)
    };
    self.emit_message(
      Some(turn_id.clone()),
      AgentEvent::ContextSummary,
      &summary_message,
    )?;

    let summary_ref = self.trace.put_payload(summary.as_bytes())?;
    self.emit(
      Some(turn_id.clone()),
      AgentEvent::ContextCompactionEpoch(ContextCompactionEpoch {
        context_epoch: self.context_epoch,
        summary: summary_ref,
        replaces_from,
        replaces_through,
      }),
    )?;
    // The summary stands where the messages it replaces stood, ahead of the
    // retained tail: the order a reader meets the context in is the order
    // the context means it.
    self.messages.drain(..removed);
    match summary_position {
      Some(position) => self.messages.insert(position - removed, summary_message),
      None => self.messages.insert(self.messages.len() - kept, summary_message),
    }
    self.last_compaction = Some(Instant::now());
    self.emit(
      Some(turn_id.clone()),
      AgentEvent::ContextCompactionCompleted(ContextCompactionCompleted {
        level: ContextLevel::L1Ordinary,
        removed_messages: removed as u32,
        retained_messages: kept as u32,
        context_epoch: self.context_epoch,
      }),
    )?;
    Ok(removed as u32)
  }

  /// Ask the model for a summary of the older history and compact the live
  /// context down to it plus a small retained tail.
  ///
  /// A dedicated request, never the turn's own round: the answer is context
  /// maintenance, not the user's answer, and it must not be mistaken for a
  /// committed reply. Nothing has been streamed when this runs — that is what
  /// makes the re-issue legal — but the budget is spent honestly, so a turn
  /// that summarizes spends one of its requests on the summary.
  /// The request the turn would build if the oldest `oldest` messages were
  /// replaced by one placeholder of summary size.

  /// The summarization request for one prefix: the prefix, the instruction,
  /// and the same system prompt the turn itself carries.
  fn summary_ask(&self, oldest: usize, preamble: &Message) -> ModelRequest {
    // The prefix rides whole, and the instruction asks at its end: what
    // the provider is offered is what the prefix is, and what it cannot
    // answer it answers with a refusal — which the summarization meets
    // with the fixed text it cannot refuse.
    let end = oldest.min(self.messages.len());
    let mut messages = self.messages[..end].to_vec();
    messages.push(preamble.clone());
    let mut request =
      ModelRequest::new(self.active_model(), self.provider().capabilities(), messages)
        .with_thinking(self.thinking);
    if let Some(system) = self.system.clone() {
      request = request.with_system(system);
    }
    request
  }

  /// Ask the model for the summary that a refused request needs in its place.
  ///
  /// The request is built like the turn's own — same system prompt, same
  /// history — with one trailing user message asking for a summary. The
  /// request itself is a context request, not a tool request, so no tools are
  /// exposed. It streams like any other round and it is charged to the turn's
  /// request budget: the budget is what the caller pays for, and maintenance
  /// spends it like an answer does.
  fn summarize_oldest(
    &mut self,
    oldest: usize,
    turn_id: &TurnId,
    scope: &TurnId,
    cancel: &CancelToken,
    progress: &mut dyn TurnProgress,
  ) -> Result<bool, TurnError> {
    let total = self.messages.len();
    let ask_limit = self.provider().capabilities().context_window;
    let preamble = Message::user(SUMMARY_PREAMBLE);
    // The prefix follows the history *as asked*: the question this turn was
    // asked with stands at the end of the history, and maintenance exists to
    // let that same request be carried again, not to summarize the question
    // away and re-ask nothing.
    let question_last = self.messages.last().is_some_and(|last| {
      !self.turn_question.is_empty() && last.text() == self.turn_question
    });
    let end = if question_last && total > 1 {
      total - 1
    } else {
      total
    };
    // An empty prefix from the caller is not "summarize nothing kept": it is
    // the whole history, the only one a window that small can ask about.
    let oldest = if oldest == 0 { end } else { oldest }.min(end).max(1);
    eprintln!("SUM oldest={oldest} real={} askfits={}", self.messages.len(),
      estimate_messages(&self.summary_ask(oldest, &preamble).messages) <= ask_limit);
    // A window that cannot carry the prefix beside the instruction is asked
    // the instruction alone, and its fixed text is what compaction keeps —
    // the summary is *expected*, not absent. A window that can carry it is
    // asked in full: an ask that drops the newest messages would throw away
    // exactly what a small window cannot summarize around, and a re-issue
    // planned beside messages the prefix never dropped would be a candidate
    // that was never there to re-issue.
    // The ask is the whole prefix beside the instruction — never a slice of
    // it, which would silently drop the newest messages a small window
    // cannot summarize around — and if that crosses the window, the ask is
    // refused and the fixed text says so.
    let request = self.summary_ask(oldest, &preamble);
    let prefix: Vec<Message> = self.messages[..oldest].to_vec();
    // An instruction the window cannot carry is no ask at all: the request
    // would be refused before the history was ever offered, and the fixed
    // text would say only what the arithmetic could have said for free.
    // Where the instruction alone does not fit, there is no summarization
    // to attempt, and the caller's refusal stands untouched.
    if estimate_messages(&[Message::user(SUMMARY_INSTRUCTION)]) > ask_limit {
      eprintln!("INSTRUCTION DOES NOT FIT");
      return Ok(false);
    }
    // The summarization itself is a *request*, and the turn loop is bounded
    // by requests: the budget is *checked* here, not merely spent. A loop
    // that let maintenance consume the last request would end the turn with
    // a summary nobody asked an answer for.
    if self.requests.load(Ordering::SeqCst) >= self.max_requests {
      self.diagnostic(
        Some(turn_id.clone()),
        DiagnosticLevel::Error,
        "request budget exhausted before the summarization it needed",
      )?;
      return Err(TurnError::Exhausted);
    }
    // The caller decides whether maintenance can help; this is the single
    // place a summarization request is spent, and the budget is *checked*
    // here, never merely spent.
    self.requests.fetch_add(1, Ordering::SeqCst);
    let epoch = self.epoch_index();
    let model = self.active_model();
    self.emit(
      Some(turn_id.clone()),
      AgentEvent::ModelRequestStarted(ModelRequestStarted {
        epoch,
        model: model.clone(),
        message_count: request.messages.len() as u32,
        context_tokens_est: estimate_tokens(&request),
        tools_exposed: 0,
      }),
    )?;
    progress.on_request_started(&model);
    let clock = Instant::now();
    let attribution = StreamAttribution {
      turn_id: turn_id.clone(),
      session_id: self.session_id.clone(),
      trace_id: self.trace_id.clone(),
      epoch,
      model,
    };
    // Progress is deliberately not wired to the deltas: a summary is not an
    // answer, and streaming it would read as if the model had replied.
    let mut silent = SilentProgress;
    // The provider is read before the collector, which borrows the trace: the
    // same split the turn's own attempt loop uses.
    let provider = self.provider();
    // The candidate's history, held aside while the ask is answered: the
    // placeholder at the prefix's place, the whole tail after it verbatim.
    let placeholder = summary_stand_in(ask_limit);
    let mut candidate_history = self.messages[..oldest].to_vec();
    candidate_history.push(placeholder.clone());
    candidate_history.extend_from_slice(&self.messages[oldest + 1..]);
    let mut collector = Collector::new(&mut silent, &mut *self.trace, attribution, cancel.clone());
    let outcome = provider.stream(&request, &mut collector, cancel);
    let duration_ms = elapsed_ms(clock);
    let collector = collector;
    // The answer decides what maintenance produced. A provider that
    // *refused the summary itself* says the prefix cannot be represented
    // inside the window even densely: compacting to a text the re-issue
    // still will not fit would trade the caller's honest refusal for a
    // second, worse one. That refusal is returned, unanswered. A failed or
    // empty summarization, by contrast, is answered with a fixed text — the
    // provider did not say the history is inexpressible, only that this
    // round did not produce a summary.
    let summary = match (&outcome, &collector.text) {
      (Ok(_), text) if !text.trim().is_empty() => text.clone(),
      (Err(failure), _) if failure.kind == ModelFailureKind::ContextOverflow => {
        // The ask crossed the window: the history does not fit beside the
        // instruction. The provider has still not refused to *summarize* —
        // it refused to carry so much beside the ask — and the fixed text
        // is what maintenance keeps from it: a summary of nothing, which
        // says plainly that the window, not the model, ended the detail.
        String::new()
      }
      _ => format!(
        "Context maintenance ran at {duration_ms} ms without a usable summary; \
         treat everything before this point as compacted."
      ),
    };
    let usage = outcome.ok();
    self.emit(
      Some(turn_id.clone()),
      AgentEvent::ModelRequestCompleted(ModelRequestCompleted {
        epoch,
        model: self.active_model(),
        finish_reason: usage.as_ref().and_then(|u| u.finish_reason.clone()),
        input_tokens: usage.as_ref().and_then(|u| u.input_tokens),
        output_tokens: usage.as_ref().and_then(|u| u.output_tokens),
        duration_ms,
        tool_calls: 0,
        reasoning_provenance: None,
      }),
    )?;
    // `compact` runs against the history the candidate was measured with:
    // the prefix it stood in for is gone, the placeholder stands where it
    // stood, and the newest message is kept verbatim beside it.
    // The history goes back exactly as the candidate measured it: the
    // prefix drained, the summary in the placeholder's place and size, the
    // tail kept verbatim beside it.
    self.messages = candidate_history;
    self.compact_with(scope, summary.trim(), total - oldest, Some(oldest + 1), Some(&placeholder))?;
    Ok(true)
  }

  /// Build the model request, consulting the context policy first.
  fn build_request(&mut self, turn_id: &TurnId) -> Result<ModelRequest, TurnError> {
    let capabilities = self.provider().capabilities();
    // Maintenance decided, against the same measurement the build makes,
    // that a summarized request fits this window. Answering the full
    // history instead would decide twice, by two different questions — and
    // the second answer would refuse the request the first one built.
    if let Some(candidate) = self.maintenance_candidate.take() {
      return Ok(candidate);
    }
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
      // Once this loop has watched a request cross the window, the policy
      // stops advising and starts requiring: an explicit compact at the safe
      // boundary, not another threshold it has already misjudged.
      state.overflow_observed = self.overflow_seen;
      state
    };
    let decision = self.context.evaluate(&state);
    // The hard bound, measured as a question rather than an assumption: is
    // there any tail of this history — the newest messages, kept verbatim —
    // beside which a summary stand-in and the request's own overhead would
    // fit? If there is, maintenance has something it can keep, and the
    // refusal belongs to the code that decides maintenance. If there is
    // not, no request this window could carry exists, and the runtime says
    // so without pretending otherwise.
    if estimate_messages(&self.messages) + request_reserve(&capabilities) > capabilities.context_window
    {
      let reserve = summary_reserve(capabilities.context_window);
      let carried = (1..=self.messages.len()).rev().any(|kept| {
        let tail = &self.messages[self.messages.len() - kept..];
        reserve + estimate_messages(tail) + request_reserve(&capabilities)
          <= capabilities.context_window
      });
      if !carried {
        return Err(TurnError::Aborted(TurnStatus::Failed {
          kind: ModelFailureKind::ContextOverflow,
        }));
      }
    }
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
      // The runtime cannot summarize: that needs a model and a user decision about
      // what to keep, and the surface owns both. It can do the tier below
      // summarization by itself: drop the oldest model-visible turns at this safe
      // boundary, where the fact and a recovery reference are recorded and the
      // canonical history remains in the trace untouched.
      ContextAction::Compact {
        level,
        reason,
        target_tokens,
      } => {
        // Policy asked for a smaller history. Eviction is that request
        // honoured: the oldest messages go, a recovery reference stays, and
        // the estimate moves toward the target the policy named. Whether a
        // *summarization* follows is decided once, by the code that owns the
        // request budget and the provider's word — never by this pass,
        // which has already rewritten the history an intercept would have
        // measured its ask against.
        if self.evict_oldest(target_tokens, turn_id)? == 0 {
          self.diagnostic(
            None,
            DiagnosticLevel::Warn,
            format!("context suggests {} compaction: {reason}", level.as_str()),
          )?;
        }
      }
      ContextAction::Warn { .. }
      | ContextAction::Keep
      | ContextAction::ReducePayload { .. }
      | ContextAction::SuggestCheckpoint { .. } => {}
    }

    // Tool schemas are exposed only when the active model can honour them: a model
    // without tool support must not be offered a call it will malform.
    let tools = if capabilities.tools {
      self.tools.specs()
    } else {
      Vec::new()
    };
    let window = capabilities.context_window;
    let mut request = ModelRequest::new(self.active_model(), capabilities, self.messages.clone())
      .with_tools(tools)
      .with_thinking(self.thinking);
    if let Some(system) = self.system.clone() {
      request = request.with_system(system);
    }
    let _ = window;
    Ok(request)
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
      self.emit(
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
        self.emit_message(
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
        self.messages.push(message);
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
      let block = self.record_tool_outcome(turn_id.clone(), call, &executed.0, executed.1)?;
      progress.on_tool_finished(call, &executed.0);
      self.messages.push(Message::new(
        Role::Tool,
        vec![ContentBlock::ToolResult(block)],
      ));
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
  ) -> Result<ToolResultBlock, TurnError> {
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
          recovery_ref,
          blob,
          tool_call_id: Some(call.id.clone()),
        }),
      )?;
    }

    let mutating = !self
      .tools
      .metadata_for(&call.name)
      .map(|meta| meta.read_only)
      .unwrap_or(true);

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
    self.emit_message(
      Some(turn_id.clone()),
      event,
      &Message::new(Role::Tool, vec![ContentBlock::ToolResult(block.clone())]),
    )?;

    Ok(block)
  }
}

/// Internal failure routing for one request attempt.
enum TurnFailure {
  /// The user stopped it. Not a fault, so never recovered from.
  Cancelled,
  /// No model can serve the request.
  Fatal(ModelFailure),
  /// The runtime itself refused to send: the request does not fit the window
  /// that currently binds it. Not a provider failure — nothing was asked.
  Aborted(TurnError),
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
      // An aborted turn is not a provider failure and never becomes one: it is
      // the runtime's own refusal, told as itself. Exhaustion is the same
      // kind of fact about the budget, not about the model.
      TurnError::Aborted(status) => Self::Aborted(TurnError::Aborted(status)),
      TurnError::Exhausted => Self::Aborted(TurnError::Exhausted),
    }
  }
}

/// The turn's own error type for a failure that never leaves as a provider
/// fault. Cancellation is the caller's command, not an error to report, and
/// the other arms cannot occur at this boundary; each still names its shape
/// rather than pretending otherwise.
fn turn_error_of(failure: TurnFailure) -> TurnError {
  match failure {
    TurnFailure::Aborted(error) => error,
    TurnFailure::Sink(error) => TurnError::Sink(error.0),
    TurnFailure::Fatal(failure) => TurnError::Unavailable(failure),
    TurnFailure::Cancelled => TurnError::Aborted(TurnStatus::Cancelled),
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
  ) -> Self {
    Self {
      progress,
      trace,
      attribution,
      cancel,
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
    bytes += spec.description.len() + spec.name.len();
  }
  (bytes / 4).max(1) as u64
}

/// Is this request close enough to the provider's own bound that asking the
/// model is worth a request: within a quarter-window of it, or over.
///
/// The two size views — the runtime's cheap estimate and the provider's real
/// limit — do not agree exactly. Summarizing whenever the estimate is merely
/// *under* the bound would ask a model for every small overflow, and asking
/// costs a request. Summarizing only near or over it means the provider is
/// consulted exactly when its answer is the only evidence that matters.


/// The summarization instruction. The ask gate measures a request carrying
/// this exact text, and the provider fixture distinguishes maintenance by it,
/// so one definition has to serve both.
const SUMMARY_PREAMBLE: &str = "Summarize in a few dense paragraphs.   Preserve the user's goal, every constraint, decisions taken, findings, and any unresolved   work. Reply with the summary alone.";

/// The summarization instruction alone, for windows that cannot hold it
/// beside even one message: the ask is then the instruction, and its
/// answer — like the fixed text that answers a failed ask — describes the
/// history it was not shown.
/// The one phrase the summary instruction always carries, and the one
/// phrase a summarized turn can never produce: it is how a provider's refusal
/// is recognized as a refusal of the *ask* rather than of the turn.
const SUMMARY_MARK: &str = "Reply with the summary alone";

const SUMMARY_INSTRUCTION: &str = "Summarize in a few dense paragraphs. Preserve the user's goal, every constraint, decisions taken, findings, and any unresolved work. Reply with the summary alone.";

/// A summary's own bound for a window: a quarter of it, and at least one
/// token, so that even a zero-token window can be maintained. A summary is
/// asked for dense and measured at its measured worst; the fraction is the
/// room maintenance leaves for it beside what survives.
fn summary_reserve(window: u64) -> u64 {
  (window / 4).max(1)
}

/// What a summary costs, carried as the message it stands in for: the
/// reserve is what a summary is *expected* to cost, and the stand-in is
/// sized in the estimator's own units — four bytes to a token — so every
/// decision about a summarized request measures the same shape the
/// re-issue will actually carry.
fn summary_stand_in(window: u64) -> Message {
  Message::system("s".repeat((summary_reserve(window) * 4) as usize))
}

/// The re-issue candidate for a reference size, named for the scan that
/// asks it twice: whether the request fits, and whether it is near enough
/// the bound to be worth the ask.
fn reissue_of(
  messages: &[Message],
  prefix: &usize,
  system: &Option<String>,
  capabilities: &ModelCapabilities,
  model: &ModelRef,
  thinking: pi_rs_core::ThinkingLevel,
) -> ModelRequest {
  summarized_candidate(messages, *prefix, system.as_ref(), capabilities, model, thinking)
}

/// The summarized request the re-issue would carry for a full history: a
/// summary stand-in in place of the prefix, the tail kept verbatim, no
/// tools, same system prompt. This is the one shape every decision about
/// what a summarized request costs — the candidate scan, the ask-size
/// shrink, and the intercept's arithmetic — has to share.

/// The summarized request the re-issue would carry: the oldest `prefix`
/// messages replaced by one summary stand-in of the reserve size, the rest
/// kept verbatim, no tools, the same system prompt. Every decision about
/// what a summarized request costs — the candidate scan, the ask shrink,
/// and the intercept's fit test — measures this one shape.
fn summarized_candidate(
  messages: &[Message],
  prefix: usize,
  system: Option<&String>,
  capabilities: &ModelCapabilities,
  model: &ModelRef,
  thinking: pi_rs_core::ThinkingLevel,
) -> ModelRequest {
  // The scan plans the ask; compaction writes the summary, and a summary
  // longer than the stand-in the scan walked would make the plan a promise
  // the history does not keep. The *first* summary of a compaction is what
  // the request carries: the stand-in stands only for what is being asked
  // away, not for what compaction already wrote.
  if messages.first().is_some_and(|first| {
    first.role == pi_rs_core::Role::System && first.text().contains(SUMMARY_MARK)
  }) {
    let mut kept = messages.to_vec();
    if prefix > 0 {
      kept.splice(..prefix, std::iter::empty());
    }
    let mut request =
      ModelRequest::new(model.clone(), capabilities.clone(), kept).with_thinking(thinking);
    if let Some(system) = system {
      request = request.with_system(system.clone());
    }
    return request;
  }
  let mut rebuilt = Vec::with_capacity(messages.len() - prefix + 1);
  rebuilt.push(summary_stand_in(capabilities.context_window));
  rebuilt.extend_from_slice(&messages[prefix..]);
  let mut request =
    ModelRequest::new(model.clone(), capabilities.clone(), rebuilt).with_thinking(thinking);
  if let Some(system) = system {
    request = request.with_system(system.clone());
  }
  request
}

/// What a request carries beside its messages: the system prompt and the
/// tool schemas are measured with the history, not after it.
fn request_reserve(capabilities: &pi_rs_core::ModelCapabilities) -> u64 {
  let _ = capabilities;
  1
}

/// Rough token estimate for history alone.
fn estimate_messages(messages: &[Message]) -> u64 {
  let bytes: usize = messages.iter().map(estimate_message_bytes).sum();
  (bytes / 4).max(1) as u64
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
    /// Every diagnostic message, newest last, whatever the event's kind.
    fn events_matching(&self, needle: &str) -> Vec<String> {
      self
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, _, payload)| {
          payload
            .to_string()
            .to_lowercase()
            .contains(&needle.to_lowercase())
        })
        .map(|(_, _, payload)| payload.to_string())
        .collect()
    }

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

  /// A provider that refuses whole requests, in the listed order, and
  /// answers whatever rounds remain beside them. A refusal is not an event
  /// a provider emits: it is the failure the request ends with.
  struct RefusingOnce {
    failures: Mutex<Vec<ModelFailureKind>>,
    answers: Scripted,
    window: u64,
    served: AtomicUsize,
  }

  impl RefusingOnce {
    fn new(failures: Vec<ModelFailureKind>) -> Self {
      RefusingOnce {
        failures: Mutex::new(failures),
        answers: Scripted::new("overflow", vec![text("the answer")]),
        window: Scripted::new("overflow", Vec::new())
          .capabilities()
          .context_window,
        served: AtomicUsize::new(0),
      }
    }

    fn window(mut self, window: u64) -> Self {
      self.window = window;
      self
    }

    fn with_answer(mut self, answer: &str) -> Self {
      self.answers = Scripted::new("overflow", vec![text(answer)]);
      self
    }

    /// The fixture answers summary requests itself, honestly and from its
    /// own window: an ask that does not fit is refused the way any other
    /// request would be. The scripted rounds are spent on the turn alone.
    fn answers_summaries(self) -> Self {
      self
    }

    /// Requests the fixture actually answered, summaries included.
    fn answered(&self) -> usize {
      self.served.load(Ordering::SeqCst)
    }
  }

  impl ModelProvider for RefusingOnce {
    fn provider_id(&self) -> &str {
      "test"
    }

    fn model(&self) -> &ModelRef {
      static MODEL: std::sync::OnceLock<ModelRef> = std::sync::OnceLock::new();
      MODEL.get_or_init(|| ModelRef::new("test", "overflow"))
    }

    fn capabilities(&self) -> ModelCapabilities {
      ModelCapabilities {
        context_window: self.window,
        ..self.answers.capabilities()
      }
    }

    fn stream(
      &self,
      request: &ModelRequest,
      sink: &mut dyn ProviderEventSink,
      _cancel: &CancelToken,
    ) -> Result<CompletionUsage, ModelFailure> {
      if is_summary_request(request) {
        // A summary is answered, not scripted: the scripted rounds belong
        // to the turn. One answered request, one sentence.
        self.served.fetch_add(1, Ordering::SeqCst);
        sink.emit(&ProviderEvent::TextDelta("summarized.".into()));
        return Ok(CompletionUsage::unknown());
      }
      let mut failures = self.failures.lock().unwrap();
      if !failures.is_empty() {
        return Err(ModelFailure::new(
          failures.remove(0),
          FailurePhase::WaitingForResponse,
          "context overflow",
        ));
      }
      drop(failures);
      self.served.fetch_add(1, Ordering::SeqCst);
      self.answers.stream(request, sink, _cancel)
    }
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
      "the summary stands ahead of the one message retained across it"
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
      epoch["replaces_from"], 1,
      "the summary replaces the session's first record"
    );
    // Session start, turn start, request start, request end, turn end, plus the
    // summary: the summary event is the last record compaction replaced.
    assert_eq!(epoch["replaces_through"], 7);
    let summary = epoch["summary"]
      .as_object()
      .expect("the epoch carries a stored blob reference, not prose");
    assert!(
      summary["hash"]
        .as_str()
        .is_some_and(|hash| hash.len() == 64),
      "the reference names stored bytes by digest: {summary:?}"
    );
  }
  /// Behaves like the providers it stands in for: a request that crosses the
  /// window is refused, anything else is answered from the next scripted
  /// round. The refusals are therefore *caused* by the history, never played.
  /// Behaves like the providers it stands in for: a request that crosses the
  /// window is refused, anything else is answered from the next scripted
  /// round. The refusals are therefore *caused* by the history, never played.
  struct OverflowProvider {
    window: u64,
    template: ModelCapabilities,
    rounds: Vec<String>,
    asks: AtomicUsize,
    answers: AtomicUsize,
  }

  impl OverflowProvider {
    fn summary_text(&self) -> &str {
      "su"
    }

    /// The provider refuses the summarization ask when even that — a prefix
    /// no longer than half its window — crosses the window: maintenance can
    /// then never produce a summary at all.
    fn summary_too_big(&self) -> bool {
      self.window < 2
    }

    fn new(window: u64, rounds: Vec<String>) -> Self {
      Self {
        window,
        template: Scripted::new("overflow", Vec::new()).capabilities(),
        rounds,
        asks: AtomicUsize::new(0),
        answers: AtomicUsize::new(0),
      }
    }
  }

  /// What the provider's own bound is on a request: tokens, four bytes to
  /// a token, as the runtime's estimator counts them.
  fn request_bytes(request: &ModelRequest) -> u64 {
    estimate_messages(&request.messages)
  }

  /// True for the request [`summarize_oldest`] sends: the instruction is the
  /// last message, and it asks for a summary. A fixture that shares one
  /// ordered list of rounds between a turn and the maintenance inside it has
  /// to tell the two requests apart, or a summary is served where an answer
  /// belongs — the precise confusion the runtime must not create.
  fn is_summary_request(request: &ModelRequest) -> bool {
    request
      .messages
      .last()
      .is_some_and(|m| m.text().contains(super::SUMMARY_MARK))
  }

  impl ModelProvider for OverflowProvider {
    fn provider_id(&self) -> &str {
      "test"
    }

    fn model(&self) -> &ModelRef {
      static MODEL: std::sync::OnceLock<ModelRef> = std::sync::OnceLock::new();
      MODEL.get_or_init(|| ModelRef::new("test", "overflow"))
    }

    fn capabilities(&self) -> ModelCapabilities {
      ModelCapabilities {
        context_window: self.window,
        ..self.template.clone()
      }
    }

    #[allow(clippy::result_large_err)]
    fn stream(
      &self,
      request: &ModelRequest,
      sink: &mut dyn ProviderEventSink,
      _cancel: &CancelToken,
    ) -> Result<CompletionUsage, ModelFailure> {
      self.asks.fetch_add(1, Ordering::SeqCst);
      // Maintenance is answered from its own text, so the turn's rounds are
      // never consumed by the summarization inside it.
      // A maintenance request is answered at the size the runtime expects a
      // summary to weigh — never at the size of the prefix it summarizes,
      // which is the very bulk the request exists to replace. The provider
      // that refuses at its window refuses the *asked* request; a summary
      // asked inside its window is answered.
      if is_summary_request(request) {
        // The instruction alone is answerable at any window: it asks the
        // model to compact, and the fixed text stands when it cannot. The
        // summary_too_big bound belongs to the full-ask fixture only.
        let instruction_only = request.messages.len() == 1
          && request.messages[0]
            .text()
            .starts_with("Summarize the conversation");
        if !instruction_only && (request_bytes(request) > self.window || self.summary_too_big()) {
          return Err(ModelFailure::new(
            ModelFailureKind::ContextOverflow,
            pi_rs_core::FailurePhase::WaitingForResponse,
            "context overflow",
          ));
        }
        sink.emit(&ProviderEvent::TextDelta(self.summary_text().to_string()));
        return Ok(CompletionUsage {
          input_tokens: None,
          output_tokens: None,
          finish_reason: None,
          certainty: pi_rs_core::CompletionCertainty::Certain,
        });
      }
      if request_bytes(request) > self.window {
        return Err(ModelFailure::new(
          ModelFailureKind::ContextOverflow,
          pi_rs_core::FailurePhase::WaitingForResponse,
          "context overflow",
        ));
      }
      let round = self.answers.fetch_add(1, Ordering::SeqCst);
      let served = self.rounds[round.min(self.rounds.len() - 1)].clone();
      sink.emit(&ProviderEvent::TextDelta(served));
      Ok(CompletionUsage {
        input_tokens: None,
        output_tokens: None,
        finish_reason: None,
        certainty: pi_rs_core::CompletionCertainty::Certain,
      })
    }
  }

  /// Streams `text`, then refuses for size: a crossing the provider reports
  /// over content the user has already seen.
  struct MidStreamOverflow {
    template: ModelCapabilities,
    text: String,
  }

  impl MidStreamOverflow {
    fn new(text: &str) -> Self {
      Self {
        template: Scripted::new("mid", Vec::new()).capabilities(),
        text: text.into(),
      }
    }
  }

  impl ModelProvider for MidStreamOverflow {
    fn provider_id(&self) -> &str {
      "test"
    }

    fn model(&self) -> &ModelRef {
      static MODEL: std::sync::OnceLock<ModelRef> = std::sync::OnceLock::new();
      MODEL.get_or_init(|| ModelRef::new("test", "mid"))
    }

    fn capabilities(&self) -> ModelCapabilities {
      self.template.clone()
    }

    #[allow(clippy::result_large_err)]
    fn stream(
      &self,
      _request: &ModelRequest,
      sink: &mut dyn ProviderEventSink,
      _cancel: &CancelToken,
    ) -> Result<CompletionUsage, ModelFailure> {
      sink.emit(&ProviderEvent::TextDelta(self.text.clone()));
      Err(ModelFailure::new(
        ModelFailureKind::ContextOverflow,
        pi_rs_core::FailurePhase::WaitingForResponse,
        "context overflow mid-stream",
      ))
    }
  }

  /// Histories whose size is arithmetic rather than fixture archaeology: a
  /// text block counts its bytes, the estimate divides by four, so four
  /// bytes of text is a token and a message under four bytes is one.
  fn filler_history_exact(messages: usize) -> Vec<Message> {
    (0..messages).map(|_| Message::user("xxxx")).collect::<Vec<_>>()
  }

  fn refused(error: &TurnError) -> bool {
    matches!(
      error,
      TurnError::Aborted(TurnStatus::Failed { kind })
        if *kind == ModelFailureKind::ContextOverflow
    )
  }

  #[test]
  fn a_second_refusal_ends_the_turn_instead_of_summarizing_again() {
    // One message fits its window; three do not. Maintenance is possible —
    // a prefix can be summarized and asked — and the provider answers, yet
    // the summarized history *alone* still crosses the window, because a
    // summary costs its reserve even when it replaces one token. The loop
    // reports the second refusal rather than burn a second summarization on
    // a question it already asked.
    // A fifth message keeps the history over the window; the newest four
    // messages are what survives summarization, and the question beside them
    // fits: the prefix that maintenance keeps is a *prefix*, not the whole
    // history a window has already refused.
    let history = filler_history_exact(5);

    // A provider that refuses every turn request: the first refusal asks
    // for maintenance, and the refused re-issue of the summarized history
    // ends the turn with the second one.
    let provider = RefusingOnce::new(vec![
      ModelFailureKind::ContextOverflow,
      ModelFailureKind::ContextOverflow,
    ])
    .window(8);
    let tools = registry_with(Vec::new());
    let policy = pi_rs_core::ProfilePolicy::new(pi_rs_core::ContextProfile::Balanced, 2);
    let mut trace = Recorder::default();
    let mut harness = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    );
    harness.messages_mut().extend(history);

    let error = harness
      .run_turn("x", &CancelToken::new(), &mut SilentProgress)
      .expect_err("the second refusal is the turn's result");
    assert!(refused(&error), "{error:?}");
    for line in trace.diagnostics() {
      eprintln!("DIAG2 {line}");
    }
    assert_eq!(
      trace.count("context_compaction_epoch"),
      1,
      "exactly the summarization's epoch — the refusal that follows reports, it does not reduce"
    );
  }

  #[test]
  fn a_summarization_is_not_an_answer_when_the_turn_cannot_be_issued() {
    // The newest turn alone crosses the window: no prefix leaves a request
    // that fits, so there is nothing maintenance can do. The runtime's own
    // refusal stands, and no text that a summarization might have produced
    // is ever passed off as the turn's answer.
    let history = filler_history_exact(1);
    assert_eq!(estimate_messages(&history), 1);
    let provider = OverflowProvider::new(0, vec!["never reached".into()]);
    let tools = registry_with(Vec::new());
    let policy = pi_rs_core::ProfilePolicy::new(pi_rs_core::ContextProfile::Balanced, 3);
    let mut trace = Recorder::default();
    let mut harness = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    );
    harness.messages_mut().extend(history);

    let error = harness
      .run_turn("late question", &CancelToken::new(), &mut SilentProgress)
      .expect_err("a history that cannot fit any window is refused");
    assert!(refused(&error), "{error:?}");
    assert_eq!(
      provider.asks.load(Ordering::SeqCst),
      0,
      "the refusal is the runtime's own: nothing was asked of the model"
    );
  }

  #[test]
  fn an_overflowed_request_is_answered_by_summarizing_and_the_turn_continues() {
    // The provider refuses the first request outright, and maintenance is
    // what makes the next one answerable: the summarization speaks, the
    // re-issue is answered, and no epoch — which would say a model stopped
    // answering — is opened for maintenance.
    //
    // The window is the provider's own: five tokens of history inside a
    // window of ninety-six is not an overflow by any measurement, and the
    // fixture refuses the turn anyway. That is the situation this path
    // exists for — the request goes because the estimate says it fits, and
    // the provider's word is what maintenance answers — and it is also the
    // only window small enough that summarizing one of five messages is
    // something a test can see.
    let history = filler_history_exact(5);
    assert_eq!(estimate_messages(&history), 5);
    // The summarized re-issue is what maintenance aims at, and a fixture
    // that refused its own answer would prove nothing about that request
    // being answered: the answer fits inside the window.
    let provider = RefusingOnce::new(vec![ModelFailureKind::ContextOverflow])
      .window(8)
      .answers_summaries().with_answer("the answer");
    let tools = registry_with(Vec::new());
    let policy = pi_rs_core::ProfilePolicy::new(pi_rs_core::ContextProfile::Balanced, 5);
    let mut trace = Recorder::default();
    let mut harness = TurnLoop::new(
      &provider,
      &tools,
      &policy,
      &mut trace,
      SessionId::new(),
      TraceId::new(),
    );
    harness.messages_mut().extend(history);

    let response = match harness.run_turn("late question", &CancelToken::new(), &mut SilentProgress)
    {
      Ok(response) => response,
      Err(error) => {
        for line in trace.diagnostics() {
          eprintln!("DIAG1 {line}");
        }
        for kind in trace.kinds() {
          eprintln!("KIND1 {kind}");
        }
        panic!("the turn completes after maintenance: {error:?}");
      }
    };
    assert_eq!(response.text, "the answer");
    assert_eq!(response.requests, 3, "the summary spent one request");
    assert_eq!(
      provider.answered(),
      2,
      "one refused question, one summarization, one answer"
    );
    assert_eq!(trace.count("context_compaction_epoch"), 1);
    assert_eq!(
      trace.count("model_epoch_started"),
      0,
      "summarizing is maintenance, not failover"
    );
  }


  #[test]
  fn a_mid_stream_overflow_is_reported_not_summarized() {
    // A provider that reports the window crossed *after* streaming describes
    // content already committed. No summary can take that back, so the loop
    // reports the failure exactly as the provider gave it.
    // The provider streamed content and *then* reported the crossing. The
    // committed text is why the loop may not answer this by summarizing.
    let provider = MidStreamOverflow::new("committed text");
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
    let error = harness
      .run_turn("x", &CancelToken::new(), &mut SilentProgress)
      .expect_err("the provider refused mid-stream");
    assert!(matches!(error, TurnError::Unavailable(_)), "{error:?}");
    assert_eq!(
      trace.count("context_compaction_epoch"),
      0,
      "a crossing reported over streamed content is reported, not rewritten"
    );
    assert_eq!(
      trace.count("context_compaction_epoch"),
      0,
      "committed content is never rewritten by a summary the caller never asked for"
    );
  }
}
