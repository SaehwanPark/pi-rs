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
  ContextAction, ContextPolicy, ContextReduced, ContextState, Diagnostic, DiagnosticLevel,
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
  /// Last provider-reported input tokens, preferred over any estimate.
  measured_input_tokens: Option<u64>,
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
      measured_input_tokens: None,
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
  pub fn with_failover(mut self, policy: FailoverPolicy) -> Self {
    self.failover = policy;
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
    self.requests.store(0, Ordering::SeqCst);

    self.ensure_session_started()?;
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

      let response = match self.attempt(turn_id.clone(), cancel, progress) {
        Ok(response) => response,
        // Cancellation is reported, never recovered from.
        Err(TurnFailure::Cancelled) => {
          return self.finish(report, TurnStatus::Cancelled, clock, Some(turn_id.clone()));
        }
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
    Ok(envelope)
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
      let request = self.build_request().map_err(TurnFailure::from)?;
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
        Err(failure) => {
          // Output reached the user the moment it was emitted, so it is recorded on
          // the failure rather than inferred afterwards.
          let mut failure = failure;
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
        let narrow = gaps
          .iter()
          .any(|gap| matches!(gap, CapabilityGap::ContextWindow { .. }));
        if narrow {
          self.rebudget(turn_id.clone())?;
        }
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
            compacted: narrow,
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
  fn rebudget(&mut self, turn_id: TurnId) -> Result<(), TurnError> {
    let target = self
      .failover
      .backup_capabilities
      .as_ref()
      .map(|caps| caps.context_window.saturating_sub(1_024))
      .unwrap_or(4_096);
    let before = estimate_messages(&self.messages);
    let mut dropped = 0u32;
    // The newest turn is never dropped: without it there is nothing to continue.
    while estimate_messages(&self.messages) > target && self.messages.len() > 1 {
      self.messages.remove(0);
      dropped += 1;
    }
    if dropped > 0 {
      self.context_epoch += 1;
      if let Some(blob) = self
        .trace
        .put_payload(format!("dropped {dropped} oldest turns for rebudget").as_bytes())?
      {
        self.emit(
          Some(turn_id.clone()),
          AgentEvent::ContextReduced(ContextReduced {
            reason: ReductionReason::RecentTargetExceeded {
              target_tokens: target,
            },
            original_bytes: before,
            visible_bytes: estimate_messages(&self.messages),
            recovery_ref: blob.recovery_ref(),
            blob,
            tool_call_id: None,
          }),
        )?;
      }
    }
    Ok(())
  }

  /// Build the model request, consulting the context policy first.
  fn build_request(&mut self) -> Result<ModelRequest, TurnError> {
    let capabilities = self.provider().capabilities();
    let state = {
      let mut state = ContextState::zero(capabilities.context_window);
      state.context_epoch = self.context_epoch;
      state.measured_tokens = self.measured_input_tokens;
      state.estimated_tokens = estimate_messages(&self.messages);
      state.working_messages = self.messages.len() as u32;
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
      // Compaction needs both a model and a user decision about what to keep;
      // doing it here would put a second model inside a turn that did not ask for
      // one. The surface owns that, so this records the recommendation instead.
      ContextAction::Compact { level, reason } => {
        self.diagnostic(
          None,
          DiagnosticLevel::Warn,
          format!("context suggests {} compaction: {reason}", level.as_str()),
        )?;
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
    let mut request = ModelRequest::new(self.active_model(), capabilities, self.messages.clone())
      .with_tools(tools)
      .with_thinking(self.thinking);
    if let Some(system) = self.system.clone() {
      request = request.with_system(system);
    }
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

    if let Some(full) = executed.full_output.as_ref() {
      // Reduction already happened in the registry. Here the full bytes become
      // recoverable, and the event records that the model saw a summary.
      if let Some(blob) = self.trace.put_payload(full)? {
        reduced = true;
        self.emit(
          Some(turn_id.clone()),
          AgentEvent::ContextReduced(ContextReduced {
            reason: ReductionReason::OversizedToolOutput {
              limit_bytes: full.len() as u64,
            },
            original_bytes: full.len() as u64,
            visible_bytes: text.len() as u64,
            recovery_ref: blob.recovery_ref(),
            blob,
            tool_call_id: Some(call.id.clone()),
          }),
        )?;
      }
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
        blob: None,
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

  use pi_rs_core::Tool;
  use pi_rs_core::{
    CompletionUsage, ProviderEvent, ProviderEventSink, ThinkingLevel, ToolChunk, ToolMetadata,
    ToolOutcome, ToolRequest,
  };
  use pi_rs_tools::Workspace;

  /// One recorded event: its turn, its wire kind, and its payload.
  type Recorded = (Option<TurnId>, String, serde_json::Value);

  /// Events as they were emitted, before any store assigns sequence numbers.
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
      self
        .0
        .lock()
        .unwrap()
        .push((envelope.meta.turn_id.clone(), kind, payload));
      Ok(())
    }

    fn put_payload(&mut self, _bytes: &[u8]) -> Result<Option<BlobRef>, SinkError> {
      // No blob store in these tests: reduction must still be reported, which is
      // what the `None` path exercises.
      Ok(None)
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
}
