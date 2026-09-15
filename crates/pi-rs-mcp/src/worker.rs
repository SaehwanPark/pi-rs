//! Coarse MCP server/worker mode for `pi-rs`.
//!
//! The client side of this crate speaks to other MCP servers. This module is the
//! opposite boundary: it gives an external orchestrator a small, stable control
//! plane for an agent worker without making terminal output part of the protocol.
//!
//! The worker is deliberately engine-agnostic. [`WorkerEngine`] is the adapter
//! boundary where an application supplies its headless `TurnLoop` composition;
//! [`WorkerService`] owns request serialization, run handles, cancellation, and
//! stable resource projections. Keeping those concerns separate means a provider
//! or runtime implementation cannot accidentally become the MCP contract.

use std::{
  collections::BTreeMap,
  fmt,
  io::{BufRead, BufReader, Read, Write},
  sync::atomic::{AtomicBool, Ordering},
  sync::{Arc, Mutex},
  thread,
  time::{Duration, Instant},
};

use pi_rs_core::{ModelRef, SessionId, now_millis, uuidv7};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::protocol::{
  CallToolParams, CallToolResult, InitializeResult, JsonRpcErrorObject, JsonRpcRequest,
  JsonRpcResponse, ListToolsResult, McpContent, McpToolDefinition, ServerCapabilities, ServerInfo,
};

/// Maximum time an MCP call may wait for a long-running worker job.
pub const MAX_WAIT_MS: u64 = 5_000;
/// Maximum number of items returned by one resource projection.
pub const MAX_RESOURCE_ITEMS: usize = 128;
/// Maximum JSON-lines request accepted by [`serve_stdio`].
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;

/// The lifecycle state an external orchestrator can observe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerStatus {
  Idle,
  Running,
  Cancelling,
  Completed,
  Cancelled,
  Failed,
}

impl WorkerStatus {
  fn terminal(self) -> bool {
    matches!(self, Self::Completed | Self::Cancelled | Self::Failed)
  }
}

/// A bounded, machine-readable domain failure returned inside a tool result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerError {
  pub code: String,
  pub message: String,
  pub retryable: bool,
  pub recoverable: bool,
  pub state_may_have_changed: bool,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub details: Option<Value>,
}

impl WorkerError {
  pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
    Self {
      code: code.into(),
      message: message.into(),
      retryable: false,
      recoverable: true,
      state_may_have_changed: false,
      details: None,
    }
  }

  pub fn invalid(message: impl Into<String>) -> Self {
    Self::new("invalid_params", message)
  }

  pub fn not_found(message: impl Into<String>) -> Self {
    Self::new("session_not_found", message)
  }

  pub fn with_retryable(mut self, retryable: bool) -> Self {
    self.retryable = retryable;
    self
  }

  pub fn with_state_change(mut self, changed: bool) -> Self {
    self.state_may_have_changed = changed;
    self
  }

  pub fn with_details(mut self, details: Value) -> Self {
    self.details = Some(details);
    self
  }
}

impl fmt::Display for WorkerError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{}: {}", self.code, self.message)
  }
}

impl std::error::Error for WorkerError {}

/// Cancellation shared by a running worker engine invocation.
#[derive(Debug, Clone, Default)]
pub struct WorkerCancelToken(Arc<AtomicBool>);

impl WorkerCancelToken {
  pub fn cancel(&self) {
    self.0.store(true, Ordering::Release);
  }

  pub fn is_cancelled(&self) -> bool {
    self.0.load(Ordering::Acquire)
  }
}

/// A redacted model-visible message exposed by the messages resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerMessage {
  pub role: String,
  pub text: String,
  pub epoch: u32,
  pub model: ModelRef,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub seq: Option<u64>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub external_context: Option<WorkerExternalContext>,
}

/// Durable identity for retrieved external evidence, without copying provider text
/// into the worker summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerExternalContext {
  pub provider: String,
  pub resource_id: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub citation: Option<String>,
  pub provenance: String,
}

/// A coarse trace projection. It intentionally omits raw event payloads and hidden
/// reasoning while preserving ordering, provenance labels, and model attribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerTraceEntry {
  pub seq: u64,
  pub kind: String,
  pub timestamp_ms: u64,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub epoch: Option<u32>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub model: Option<ModelRef>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub provenance: Option<String>,
}

/// A summary authored for the external worker surface. This is not reconstructed
/// reasoning and is never used as a substitute for the canonical trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerSummary {
  pub text: String,
  pub source: WorkerSummarySource,
  pub updated_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerSummarySource {
  Declared,
  Provider,
  Runtime,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerDiff {
  #[serde(default)]
  pub files: Vec<WorkerDiffFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerDiffFile {
  pub path: String,
  pub status: String,
  #[serde(default)]
  pub additions: u64,
  #[serde(default)]
  pub deletions: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerArtifact {
  pub id: String,
  pub kind: String,
  pub label: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub citation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerCheckpoint {
  pub id: String,
  pub summary: String,
  pub created_at_ms: u64,
}

/// Model epoch history is intentionally a stable projection rather than a raw event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerModelEpoch {
  pub epoch: u32,
  pub model: ModelRef,
  pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerFailover {
  pub from_epoch: u32,
  pub to_epoch: u32,
  pub reason: String,
}

/// Stable state resource for one worker session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerState {
  pub session_id: SessionId,
  pub status: WorkerStatus,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub active_run_id: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub parent_session: Option<SessionId>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub branched_from_seq: Option<u64>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub primary_model: Option<ModelRef>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub active_model: Option<ModelRef>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub backup_model: Option<ModelRef>,
  pub failed_over: bool,
  pub context_epoch: u32,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub latest_checkpoint_id: Option<String>,
  pub closed: bool,
  pub updated_at_ms: u64,
  #[serde(default)]
  pub epochs: Vec<WorkerModelEpoch>,
  #[serde(default)]
  pub failovers: Vec<WorkerFailover>,
}

/// All worker-owned state. Individual MCP resources expose projections of this
/// value, never this implementation aggregate wholesale.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WorkerExecution {
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub summary: Option<WorkerSummary>,
  #[serde(default)]
  pub messages: Vec<WorkerMessage>,
  #[serde(default)]
  pub trace: Vec<WorkerTraceEntry>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub diff: Option<WorkerDiff>,
  #[serde(default)]
  pub artifacts: Vec<WorkerArtifact>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub checkpoint: Option<WorkerCheckpoint>,
  #[serde(default)]
  pub epochs: Vec<WorkerModelEpoch>,
  #[serde(default)]
  pub failovers: Vec<WorkerFailover>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub active_model: Option<ModelRef>,
}

/// Input supplied to an engine for a turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerRunRequest {
  pub session_id: SessionId,
  pub prompt: String,
  pub continuation: bool,
  pub prior_state: WorkerState,
  pub prior_messages: Vec<WorkerMessage>,
}

/// Input supplied to an engine for explicit compaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerCompactRequest {
  pub session_id: SessionId,
  pub mode: WorkerCompactMode,
  pub phase: Option<String>,
  pub summary: Option<String>,
  pub force: bool,
  pub prior_state: WorkerState,
  pub prior_messages: Vec<WorkerMessage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(clippy::enum_variant_names)]
pub enum WorkerCompactMode {
  #[default]
  Conversation,
  Phase,
  Checkpoint,
}

/// The headless runtime adapter supplied by an embedding application.
///
/// An implementation normally composes `TurnLoop`, `StoreTrace`, `CancelToken`,
/// and `SilentProgress`. The MCP layer does not know provider details and therefore
/// cannot accidentally expose them as a public protocol.
pub trait WorkerEngine: Send + Sync + 'static {
  fn run(
    &self,
    request: WorkerRunRequest,
    cancel: WorkerCancelToken,
  ) -> Result<WorkerExecution, WorkerError>;

  fn compact(&self, request: WorkerCompactRequest) -> Result<WorkerExecution, WorkerError> {
    let summary = request.summary.map(|text| WorkerSummary {
      text,
      source: WorkerSummarySource::Declared,
      updated_at_ms: now_millis(),
    });
    Ok(WorkerExecution {
      summary,
      ..WorkerExecution::default()
    })
  }
}

#[derive(Debug, Clone)]
struct WorkerSession {
  state: WorkerState,
  summary: Option<WorkerSummary>,
  messages: Vec<WorkerMessage>,
  trace: Vec<WorkerTraceEntry>,
  diff: Option<WorkerDiff>,
  artifacts: Vec<WorkerArtifact>,
  checkpoint: Option<WorkerCheckpoint>,
  cancel: Option<WorkerCancelToken>,
}

struct WorkerInner<E> {
  engine: Arc<E>,
  sessions: Mutex<BTreeMap<String, WorkerSession>>,
}

/// Cloneable stateful worker control plane.
pub struct WorkerService<E> {
  inner: Arc<WorkerInner<E>>,
}

impl<E> Clone for WorkerService<E> {
  fn clone(&self) -> Self {
    Self {
      inner: Arc::clone(&self.inner),
    }
  }
}

impl<E> fmt::Debug for WorkerService<E> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("WorkerService").finish_non_exhaustive()
  }
}

impl<E: WorkerEngine> WorkerService<E> {
  pub fn new(engine: E) -> Self {
    Self {
      inner: Arc::new(WorkerInner {
        engine: Arc::new(engine),
        sessions: Mutex::new(BTreeMap::new()),
      }),
    }
  }

  pub fn start(&self, request: StartRequest) -> Result<WorkerRunHandle, WorkerError> {
    if request.prompt.trim().is_empty() {
      return Err(WorkerError::invalid("prompt must not be empty"));
    }
    let session_id = SessionId::new();
    let state = WorkerState {
      session_id: session_id.clone(),
      status: WorkerStatus::Idle,
      active_run_id: None,
      parent_session: None,
      branched_from_seq: None,
      primary_model: None,
      active_model: None,
      backup_model: None,
      failed_over: false,
      context_epoch: 0,
      latest_checkpoint_id: None,
      closed: false,
      updated_at_ms: now_millis(),
      epochs: Vec::new(),
      failovers: Vec::new(),
    };
    self.insert_session(WorkerSession {
      state,
      summary: None,
      messages: Vec::new(),
      trace: Vec::new(),
      diff: None,
      artifacts: Vec::new(),
      checkpoint: None,
      cancel: None,
    })?;
    self.begin_run(session_id, request.prompt, false, request.wait_ms)
  }

  pub fn continue_session(&self, request: ContinueRequest) -> Result<WorkerRunHandle, WorkerError> {
    if request.prompt.trim().is_empty() {
      return Err(WorkerError::invalid("prompt must not be empty"));
    }
    let session_id = parse_session_id(&request.session_id)?;
    self.begin_run(session_id, request.prompt, true, request.wait_ms)
  }

  pub fn cancel(&self, request: CancelRequest) -> Result<CancelResult, WorkerError> {
    let session_id = parse_session_id(&request.session_id)?;
    let mut sessions = self.lock_sessions()?;
    let session = sessions.get_mut(session_id.as_str()).ok_or_else(|| {
      WorkerError::not_found(format!("session {} was not found", request.session_id))
    })?;
    let Some(active_run_id) = session.state.active_run_id.clone() else {
      if !session.state.status.terminal() {
        return Err(WorkerError::new(
          "not_cancellable",
          "session has no active run",
        ));
      }
      return Ok(CancelResult {
        session_id: session.state.session_id.clone(),
        run_id: None,
        status: session.state.status,
        already_terminal: true,
      });
    };
    if request
      .run_id
      .as_deref()
      .is_some_and(|run| run != active_run_id)
    {
      return Err(WorkerError::new("run_not_found", "run id is not active"));
    }
    if let Some(cancel) = &session.cancel {
      cancel.cancel();
    }
    session.state.status = WorkerStatus::Cancelling;
    session.state.updated_at_ms = now_millis();
    Ok(CancelResult {
      session_id: session.state.session_id.clone(),
      run_id: Some(active_run_id),
      status: WorkerStatus::Cancelling,
      already_terminal: false,
    })
  }

  pub fn branch(&self, request: BranchRequest) -> Result<WorkerRunHandle, WorkerError> {
    let source_id = parse_session_id(&request.session_id)?;
    let source = {
      let sessions = self.lock_sessions()?;
      sessions
        .get(&request.session_id)
        .cloned()
        .ok_or_else(|| WorkerError::not_found("source session was not found"))?
    };
    if source.state.status == WorkerStatus::Running
      || source.state.status == WorkerStatus::Cancelling
    {
      return Err(WorkerError::new(
        "session_busy",
        "cannot branch a running session",
      ));
    }
    if let Some(kind) = request.from_kind.as_deref() {
      if kind == "event" {
        return Err(WorkerError::new(
          "unsupported",
          "historical event branching belongs to the replay boundary",
        ));
      }
      if kind == "checkpoint" {
        return Err(WorkerError::new(
          "unsupported",
          "checkpoint branching requires an engine checkpoint snapshot",
        ));
      }
      if kind != "latest" {
        return Err(WorkerError::invalid("branch from must be latest"));
      }
    }
    let new_id = SessionId::new();
    let mut state = source.state.clone();
    state.session_id = new_id.clone();
    state.status = WorkerStatus::Idle;
    state.active_run_id = None;
    state.parent_session = Some(source_id);
    state.branched_from_seq = source.trace.last().map(|entry| entry.seq);
    state.closed = false;
    state.updated_at_ms = now_millis();
    let session = WorkerSession {
      state,
      summary: source.summary,
      messages: source.messages,
      trace: source.trace,
      diff: source.diff,
      artifacts: source.artifacts,
      checkpoint: source.checkpoint,
      cancel: None,
    };
    self.insert_session(session)?;
    match request.prompt {
      Some(prompt) if !prompt.trim().is_empty() => {
        self.begin_run(new_id, prompt, true, request.wait_ms)
      }
      Some(_) => Err(WorkerError::invalid("branch prompt must not be empty")),
      None => Ok(self.handle_for(&new_id, None)?),
    }
  }

  pub fn compact(&self, request: CompactRequest) -> Result<CompactResult, WorkerError> {
    let session_id = parse_session_id(&request.session_id)?;
    let (state, messages) = {
      let sessions = self.lock_sessions()?;
      let session = sessions
        .get(&request.session_id)
        .ok_or_else(|| WorkerError::not_found("session was not found"))?;
      if session.state.status == WorkerStatus::Running
        || session.state.status == WorkerStatus::Cancelling
      {
        return Err(WorkerError::new(
          "session_busy",
          "cannot compact a running session",
        ));
      }
      (session.state.clone(), session.messages.clone())
    };
    let mode = request.mode.parse().map_err(WorkerError::invalid)?;
    let execution = self.inner.engine.compact(WorkerCompactRequest {
      session_id: session_id.clone(),
      mode,
      phase: request.phase,
      summary: request.summary,
      force: request.force,
      prior_state: state,
      prior_messages: messages,
    })?;
    let mut sessions = self.lock_sessions()?;
    let session = sessions
      .get_mut(&request.session_id)
      .ok_or_else(|| WorkerError::not_found("session was removed during compaction"))?;
    self.apply_execution(session, execution);
    session.state.context_epoch = session.state.context_epoch.saturating_add(1);
    session.state.updated_at_ms = now_millis();
    if session.state.status == WorkerStatus::Idle {
      session.state.status = WorkerStatus::Completed;
    }
    Ok(CompactResult {
      session_id,
      status: session.state.status,
      context_epoch: session.state.context_epoch,
      checkpoint_id: session
        .checkpoint
        .as_ref()
        .map(|checkpoint| checkpoint.id.clone()),
    })
  }

  pub fn snapshot(&self, session_id: &str) -> Result<WorkerSnapshot, WorkerError> {
    let sessions = self.lock_sessions()?;
    let session = sessions
      .get(session_id)
      .ok_or_else(|| WorkerError::not_found("session was not found"))?;
    Ok(WorkerSnapshot {
      state: session.state.clone(),
      summary: session.summary.clone(),
      messages: session.messages.clone(),
      trace: session.trace.clone(),
      diff: session.diff.clone(),
      artifacts: session.artifacts.clone(),
      checkpoint: session.checkpoint.clone(),
    })
  }

  /// Read one stable `session://` resource as JSON.
  pub fn read_resource(&self, uri: &str) -> Result<Value, WorkerError> {
    self.resource_value(uri)
  }

  /// Dispatch one canonical agent tool by name. This is also useful to adapters
  /// that already have an MCP transport and do not need [`WorkerMcpServer`].
  pub fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, WorkerError> {
    match name {
      "agent.start" => {
        let request: StartRequest = serde_json::from_value(arguments)
          .map_err(|error| WorkerError::invalid(error.to_string()))?;
        serde_json::to_value(self.start(request)?).map_err(encode_error)
      }
      "agent.continue" => {
        let request: ContinueRequest = serde_json::from_value(arguments)
          .map_err(|error| WorkerError::invalid(error.to_string()))?;
        serde_json::to_value(self.continue_session(request)?).map_err(encode_error)
      }
      "agent.cancel" => {
        let request: CancelRequest = serde_json::from_value(arguments)
          .map_err(|error| WorkerError::invalid(error.to_string()))?;
        serde_json::to_value(self.cancel(request)?).map_err(encode_error)
      }
      "agent.branch" => {
        let request: BranchRequest = serde_json::from_value(arguments)
          .map_err(|error| WorkerError::invalid(error.to_string()))?;
        serde_json::to_value(self.branch(request)?).map_err(encode_error)
      }
      "agent.compact" => {
        let request: CompactRequest = serde_json::from_value(arguments)
          .map_err(|error| WorkerError::invalid(error.to_string()))?;
        serde_json::to_value(self.compact(request)?).map_err(encode_error)
      }
      _ => Err(WorkerError::new(
        "unsupported",
        format!("unknown worker tool {name}"),
      )),
    }
  }

  fn begin_run(
    &self,
    session_id: SessionId,
    prompt: String,
    continuation: bool,
    wait_ms: Option<u64>,
  ) -> Result<WorkerRunHandle, WorkerError> {
    let run_id = uuidv7();
    let cancel = WorkerCancelToken::default();
    let request_state;
    let request_messages;
    {
      let mut sessions = self.lock_sessions()?;
      let session = sessions
        .get_mut(session_id.as_str())
        .ok_or_else(|| WorkerError::not_found("session was not found"))?;
      if session.state.closed {
        return Err(WorkerError::new("session_closed", "session is closed"));
      }
      if matches!(
        session.state.status,
        WorkerStatus::Running | WorkerStatus::Cancelling
      ) {
        return Err(WorkerError::new(
          "session_busy",
          "session already has a running job",
        ));
      }
      request_state = session.state.clone();
      request_messages = session.messages.clone();
      session.state.status = WorkerStatus::Running;
      session.state.active_run_id = Some(run_id.clone());
      session.state.updated_at_ms = now_millis();
      session.cancel = Some(cancel.clone());
    }
    let service = WorkerService {
      inner: Arc::clone(&self.inner),
    };
    let engine = Arc::clone(&self.inner.engine);
    let job_session_id = session_id.clone();
    let job_run_id = run_id.clone();
    thread::spawn(move || {
      let result = engine.run(
        WorkerRunRequest {
          session_id: job_session_id.clone(),
          prompt,
          continuation,
          prior_state: request_state,
          prior_messages: request_messages,
        },
        cancel.clone(),
      );
      service.finish_run(&job_session_id, &job_run_id, cancel.is_cancelled(), result);
    });
    let handle = self.handle_for(&session_id, Some(run_id))?;
    self.wait_handle(handle, wait_ms)
  }

  fn wait_handle(
    &self,
    mut handle: WorkerRunHandle,
    wait_ms: Option<u64>,
  ) -> Result<WorkerRunHandle, WorkerError> {
    let Some(wait_ms) = wait_ms else {
      return Ok(handle);
    };
    let deadline = Instant::now() + Duration::from_millis(wait_ms.min(MAX_WAIT_MS));
    while Instant::now() < deadline && !handle.status.terminal() {
      thread::sleep(Duration::from_millis(5));
      handle = self.handle_for(
        &parse_session_id(handle.session_id.as_str())?,
        handle.run_id.clone(),
      )?;
    }
    Ok(handle)
  }

  fn finish_run(
    &self,
    session_id: &SessionId,
    run_id: &str,
    cancelled: bool,
    result: Result<WorkerExecution, WorkerError>,
  ) {
    let Ok(mut sessions) = self.inner.sessions.lock() else {
      return;
    };
    let Some(session) = sessions.get_mut(session_id.as_str()) else {
      return;
    };
    if session.state.active_run_id.as_deref() != Some(run_id) {
      return;
    }
    session.cancel = None;
    match result {
      Ok(execution) => {
        self.apply_execution(session, execution);
        session.state.status = if cancelled {
          WorkerStatus::Cancelled
        } else {
          WorkerStatus::Completed
        };
      }
      Err(error) => {
        session.state.status = if cancelled {
          WorkerStatus::Cancelled
        } else {
          WorkerStatus::Failed
        };
        session.summary = Some(WorkerSummary {
          text: error.message,
          source: WorkerSummarySource::Runtime,
          updated_at_ms: now_millis(),
        });
      }
    }
    session.state.active_run_id = None;
    session.state.updated_at_ms = now_millis();
  }

  fn apply_execution(&self, session: &mut WorkerSession, execution: WorkerExecution) {
    if execution.summary.is_some() {
      session.summary = execution.summary;
    }
    if !execution.messages.is_empty() {
      session.messages = execution.messages;
    }
    if !execution.trace.is_empty() {
      session.trace.extend(execution.trace);
    }
    if execution.diff.is_some() {
      session.diff = execution.diff;
    }
    if !execution.artifacts.is_empty() {
      session.artifacts.extend(execution.artifacts);
    }
    if execution.checkpoint.is_some() {
      session.checkpoint = execution.checkpoint;
    }
    for epoch in execution.epochs {
      if let Some(existing) = session
        .state
        .epochs
        .iter_mut()
        .find(|existing| existing.epoch == epoch.epoch)
      {
        *existing = epoch;
      } else {
        session.state.epochs.push(epoch);
      }
    }
    session.state.epochs.sort_by_key(|epoch| epoch.epoch);
    for failover in execution.failovers {
      if !session.state.failovers.contains(&failover) {
        session.state.failovers.push(failover);
      }
    }
    if execution.active_model.is_some() {
      session.state.active_model = execution.active_model;
    }
    session.state.failed_over = !session.state.failovers.is_empty();
    session.state.latest_checkpoint_id = session
      .checkpoint
      .as_ref()
      .map(|checkpoint| checkpoint.id.clone());
  }

  fn handle_for(
    &self,
    session_id: &SessionId,
    requested_run_id: Option<String>,
  ) -> Result<WorkerRunHandle, WorkerError> {
    let sessions = self.lock_sessions()?;
    let session = sessions
      .get(session_id.as_str())
      .ok_or_else(|| WorkerError::not_found("session was not found"))?;
    Ok(WorkerRunHandle {
      session_id: session_id.to_string(),
      run_id: requested_run_id.or_else(|| session.state.active_run_id.clone()),
      status: session.state.status,
      resources: resource_uris(session_id),
    })
  }

  fn insert_session(&self, session: WorkerSession) -> Result<(), WorkerError> {
    let id = session.state.session_id.to_string();
    let mut sessions = self.lock_sessions()?;
    sessions.insert(id, session);
    Ok(())
  }

  fn lock_sessions(
    &self,
  ) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, WorkerSession>>, WorkerError> {
    self.inner.sessions.lock().map_err(|_| {
      WorkerError::new("worker_unavailable", "worker state lock is poisoned").with_retryable(true)
    })
  }

  fn resource_value(&self, uri: &str) -> Result<Value, WorkerError> {
    let (session_id, kind, query) = parse_resource_uri(uri)?;
    let snapshot = self.snapshot(session_id.as_str())?;
    let value = match kind {
      ResourceKind::State => json!(snapshot.state),
      ResourceKind::Summary => json!({
        "session_id": snapshot.state.session_id,
        "status": snapshot.state.status,
        "summary": snapshot.summary,
        "message_count": snapshot.messages.len(),
        "trace_count": snapshot.trace.len(),
      }),
      ResourceKind::Messages => page_items(snapshot.messages, query),
      ResourceKind::Trace => page_items(snapshot.trace, query),
      ResourceKind::Diff => snapshot.diff.map_or_else(
        || json!({"available": false}),
        |diff| {
          let page = page_diff(diff, query);
          json!({"available": true, "diff": page})
        },
      ),
      ResourceKind::Artifacts => {
        let available = !snapshot.artifacts.is_empty();
        json!({
          "available": available,
          "artifacts": snapshot
            .artifacts
            .into_iter()
            .take(MAX_RESOURCE_ITEMS)
            .collect::<Vec<_>>(),
        })
      }
      ResourceKind::Checkpoint => json!({
        "available": snapshot.checkpoint.is_some(),
        "checkpoint": snapshot.checkpoint,
      }),
    };
    Ok(value)
  }
}

fn encode_error(error: serde_json::Error) -> WorkerError {
  WorkerError::new("serialization", error.to_string()).with_state_change(false)
}

/// A read-only view returned by [`WorkerService::snapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerSnapshot {
  pub state: WorkerState,
  pub summary: Option<WorkerSummary>,
  pub messages: Vec<WorkerMessage>,
  pub trace: Vec<WorkerTraceEntry>,
  pub diff: Option<WorkerDiff>,
  pub artifacts: Vec<WorkerArtifact>,
  pub checkpoint: Option<WorkerCheckpoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartRequest {
  pub prompt: String,
  #[serde(default)]
  pub wait_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContinueRequest {
  pub session_id: String,
  pub prompt: String,
  #[serde(default)]
  pub wait_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelRequest {
  pub session_id: String,
  #[serde(default)]
  pub run_id: Option<String>,
  #[serde(default)]
  pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchRequest {
  pub session_id: String,
  #[serde(default)]
  pub from_kind: Option<String>,
  #[serde(default)]
  pub prompt: Option<String>,
  #[serde(default)]
  pub wait_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactRequest {
  pub session_id: String,
  #[serde(default)]
  pub mode: String,
  #[serde(default)]
  pub phase: Option<String>,
  #[serde(default)]
  pub summary: Option<String>,
  #[serde(default)]
  pub force: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerRunHandle {
  pub session_id: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub run_id: Option<String>,
  pub status: WorkerStatus,
  pub resources: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelResult {
  pub session_id: SessionId,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub run_id: Option<String>,
  pub status: WorkerStatus,
  pub already_terminal: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactResult {
  pub session_id: SessionId,
  pub status: WorkerStatus,
  pub context_epoch: u32,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub checkpoint_id: Option<String>,
}

fn parse_session_id(value: &str) -> Result<SessionId, WorkerError> {
  if value.trim().is_empty() || value.contains('/') || value.contains('\\') || value.contains("..")
  {
    return Err(WorkerError::invalid("invalid session id"));
  }
  Ok(SessionId::from_string(value))
}

fn resource_uris(session_id: &SessionId) -> Vec<String> {
  let id = session_id.as_str();
  [
    "state",
    "summary",
    "messages",
    "trace",
    "diff",
    "artifacts",
    "checkpoint/latest",
  ]
  .into_iter()
  .map(|kind| format!("session://{id}/{kind}"))
  .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResourceKind {
  State,
  Summary,
  Messages,
  Trace,
  Diff,
  Artifacts,
  Checkpoint,
}

#[derive(Debug, Clone, Copy, Default)]
struct ResourceQuery {
  after_seq: Option<u64>,
  limit: usize,
}

fn parse_resource_uri(uri: &str) -> Result<(SessionId, ResourceKind, ResourceQuery), WorkerError> {
  let (base, query_text) = uri.split_once('?').unwrap_or((uri, ""));
  let rest = base
    .strip_prefix("session://")
    .ok_or_else(|| WorkerError::new("resource_not_found", "resource URI must use session://"))?;
  let (session, path) = rest
    .split_once('/')
    .ok_or_else(|| WorkerError::new("resource_not_found", "resource URI has no path"))?;
  let session = parse_session_id(session)?;
  let kind = match path {
    "state" => ResourceKind::State,
    "summary" => ResourceKind::Summary,
    "messages" => ResourceKind::Messages,
    "trace" => ResourceKind::Trace,
    "diff" => ResourceKind::Diff,
    "artifacts" => ResourceKind::Artifacts,
    "checkpoint/latest" => ResourceKind::Checkpoint,
    _ => {
      return Err(WorkerError::new(
        "resource_not_found",
        "unknown session resource",
      ));
    }
  };
  let mut query = ResourceQuery {
    after_seq: None,
    limit: MAX_RESOURCE_ITEMS,
  };
  for pair in query_text.split('&').filter(|pair| !pair.is_empty()) {
    let (key, value) = pair
      .split_once('=')
      .ok_or_else(|| WorkerError::invalid("resource query must be key=value"))?;
    match key {
      "after_seq" => {
        query.after_seq = Some(
          value
            .parse()
            .map_err(|_| WorkerError::invalid("after_seq must be an integer"))?,
        );
      }
      "limit" => {
        query.limit = value
          .parse::<usize>()
          .map_err(|_| WorkerError::invalid("limit must be an integer"))?
          .clamp(1, MAX_RESOURCE_ITEMS);
      }
      _ => {
        return Err(WorkerError::invalid(format!(
          "unknown resource query key {key}"
        )));
      }
    }
  }
  Ok((session, kind, query))
}

fn page_items<T: Serialize>(items: Vec<T>, query: ResourceQuery) -> Value {
  let mut values = Vec::new();
  for item in items {
    let Ok(value) = serde_json::to_value(item) else {
      continue;
    };
    if let Some(after) = query.after_seq {
      let seq = value["seq"].as_u64().unwrap_or(0);
      if seq <= after {
        continue;
      }
    }
    values.push(value);
    if values.len() >= query.limit {
      break;
    }
  }
  let next_after_seq = values.last().and_then(|value| value["seq"].as_u64());
  json!({"items": values, "next_after_seq": next_after_seq})
}

/// Resource description returned by the MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerResourceDescription {
  pub uri: String,
  pub name: String,
  #[serde(rename = "mimeType")]
  pub mime_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerResourceContent {
  pub uri: String,
  #[serde(rename = "mimeType")]
  pub mime_type: String,
  pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerResourceReadResult {
  pub contents: Vec<WorkerResourceContent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerResourceListResult {
  pub resources: Vec<WorkerResourceDescription>,
}

/// MCP JSON-RPC server dispatcher for [`WorkerService`].
pub struct WorkerMcpServer<E> {
  service: WorkerService<E>,
}

impl<E> Clone for WorkerMcpServer<E> {
  fn clone(&self) -> Self {
    Self {
      service: self.service.clone(),
    }
  }
}

impl<E> fmt::Debug for WorkerMcpServer<E> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("WorkerMcpServer").finish_non_exhaustive()
  }
}

impl<E: WorkerEngine> WorkerMcpServer<E> {
  pub fn new(service: WorkerService<E>) -> Self {
    Self { service }
  }

  pub fn service(&self) -> &WorkerService<E> {
    &self.service
  }

  pub fn handle(&self, request: JsonRpcRequest) -> JsonRpcResponse {
    let id = request.id.clone();
    if request.jsonrpc != "2.0" {
      return JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id,
        result: None,
        error: Some(JsonRpcErrorObject {
          code: -32600,
          message: "jsonrpc must be 2.0".into(),
          data: None,
        }),
      };
    }
    let result = match request.method.as_str() {
      "initialize" => serde_json::to_value(InitializeResult {
        protocol_version: crate::protocol::LATEST_PROTOCOL_VERSION.into(),
        capabilities: ServerCapabilities {
          tools: Some(json!({})),
          resources: Some(json!({})),
          prompts: None,
          logging: None,
        },
        server_info: ServerInfo {
          name: "pi-rs-worker".into(),
          version: Some(env!("CARGO_PKG_VERSION").into()),
        },
      })
      .map_err(|error| rpc_internal(error.to_string())),
      "tools/list" => serde_json::to_value(ListToolsResult {
        tools: worker_tools(),
        next_cursor: None,
      })
      .map_err(|error| rpc_internal(error.to_string())),
      "tools/call" => self.handle_tool_call(request.params),
      "resources/list" => serde_json::to_value(WorkerResourceListResult {
        resources: resource_descriptions(),
      })
      .map_err(|error| rpc_internal(error.to_string())),
      "resources/read" => self.handle_resource_read(request.params),
      _ => Err(JsonRpcErrorObject {
        code: -32601,
        message: format!("method not found: {}", request.method),
        data: None,
      }),
    };
    match result {
      Ok(value) => JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id,
        result: Some(value),
        error: None,
      },
      Err(error) => JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id,
        result: None,
        error: Some(error),
      },
    }
  }

  fn handle_tool_call(&self, params: Option<Value>) -> Result<Value, JsonRpcErrorObject> {
    let params: CallToolParams = serde_json::from_value(params.unwrap_or_default())
      .map_err(|error| rpc_invalid(error.to_string()))?;
    match self
      .service
      .call_tool(&params.name, params.arguments.unwrap_or_else(|| json!({})))
    {
      Ok(value) => serde_json::to_value(CallToolResult {
        content: vec![McpContent {
          content_type: "text".into(),
          text: Some(value.to_string()),
          data: None,
          mime_type: None,
        }],
        is_error: Some(false),
      })
      .map_err(|error| rpc_internal(error.to_string())),
      Err(error) => serde_json::to_value(CallToolResult {
        content: vec![McpContent {
          content_type: "text".into(),
          text: Some(serde_json::to_string(&error).unwrap_or_else(|_| error.to_string())),
          data: None,
          mime_type: None,
        }],
        is_error: Some(true),
      })
      .map_err(|serialization| rpc_internal(serialization.to_string())),
    }
  }

  fn handle_resource_read(&self, params: Option<Value>) -> Result<Value, JsonRpcErrorObject> {
    let params: ReadResourceParams = serde_json::from_value(params.unwrap_or_default())
      .map_err(|error| rpc_invalid(error.to_string()))?;
    let value = self
      .service
      .resource_value(&params.uri)
      .map_err(|error| rpc_invalid(error.to_string()))?;
    serde_json::to_value(WorkerResourceReadResult {
      contents: vec![WorkerResourceContent {
        uri: params.uri,
        mime_type: "application/json".into(),
        text: value.to_string(),
      }],
    })
    .map_err(|error| rpc_internal(error.to_string()))
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReadResourceParams {
  uri: String,
}

fn worker_tools() -> Vec<McpToolDefinition> {
  [
    ("agent.start", "Start a new headless agent session."),
    ("agent.continue", "Continue an existing worker session."),
    (
      "agent.cancel",
      "Cancel the active run for a worker session.",
    ),
    (
      "agent.branch",
      "Create a current-state branch of a worker session.",
    ),
    (
      "agent.compact",
      "Compact a worker session at a safe boundary.",
    ),
  ]
  .into_iter()
  .map(|(name, description)| McpToolDefinition {
    name: name.into(),
    description: Some(description.into()),
    input_schema: json!({"type": "object"}),
  })
  .collect()
}

fn resource_descriptions() -> Vec<WorkerResourceDescription> {
  [
    ("state", "Session state"),
    ("summary", "External session summary"),
    ("messages", "Model-visible messages"),
    ("trace", "Coarse canonical trace projection"),
    ("diff", "Workspace diff projection"),
    ("artifacts", "Artifact references"),
    ("checkpoint/latest", "Latest checkpoint capsule"),
  ]
  .into_iter()
  .map(|(kind, name)| WorkerResourceDescription {
    uri: format!("session://<id>/{kind}"),
    name: name.into(),
    mime_type: "application/json".into(),
  })
  .collect()
}

fn rpc_invalid(message: String) -> JsonRpcErrorObject {
  JsonRpcErrorObject {
    code: -32602,
    message,
    data: None,
  }
}

fn rpc_internal(message: String) -> JsonRpcErrorObject {
  JsonRpcErrorObject {
    code: -32603,
    message,
    data: None,
  }
}

/// Serve newline-delimited JSON-RPC over a caller-owned stdio-like reader/writer.
///
/// Notifications (requests without an `id`) are accepted and produce no response;
/// this is enough for `notifications/initialized` while keeping the server usable
/// in tests with in-memory buffers.
pub fn serve_stdio<E: WorkerEngine, R: Read, W: Write>(
  server: &WorkerMcpServer<E>,
  reader: R,
  mut writer: W,
) -> Result<(), WorkerServerError> {
  let mut reader = BufReader::new(reader);
  loop {
    let Some(line) = read_bounded_line(&mut reader)? else {
      return Ok(());
    };
    let line = String::from_utf8_lossy(&line);
    let raw: Value = match serde_json::from_str(&line) {
      Ok(raw) => raw,
      Err(error) => {
        write_server_response(
          &mut writer,
          &JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: Value::Null,
            result: None,
            error: Some(JsonRpcErrorObject {
              code: -32700,
              message: error.to_string(),
              data: None,
            }),
          },
        )?;
        continue;
      }
    };
    if raw.get("id").is_none() {
      continue;
    }
    let request: JsonRpcRequest = match serde_json::from_value(raw.clone()) {
      Ok(request) => request,
      Err(error) => {
        write_server_response(
          &mut writer,
          &JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: raw.get("id").cloned().unwrap_or(Value::Null),
            result: None,
            error: Some(JsonRpcErrorObject {
              code: -32600,
              message: error.to_string(),
              data: None,
            }),
          },
        )?;
        continue;
      }
    };
    let response = server.handle(request);
    write_server_response(&mut writer, &response)?;
  }
}

fn read_bounded_line<R: BufRead>(reader: &mut R) -> Result<Option<Vec<u8>>, WorkerServerError> {
  let mut line = Vec::new();
  loop {
    let buffer = reader.fill_buf()?;
    if buffer.is_empty() {
      return if line.is_empty() {
        Ok(None)
      } else {
        Ok(Some(line))
      };
    }
    let take = buffer
      .iter()
      .position(|byte| *byte == b'\n')
      .map_or(buffer.len(), |index| index + 1);
    if line.len() + take > MAX_REQUEST_BYTES {
      return Err(WorkerServerError::RequestTooLarge);
    }
    let has_more = take < buffer.len();
    line.extend_from_slice(&buffer[..take]);
    reader.consume(take);
    if has_more {
      return Ok(Some(line));
    }
  }
}

fn page_diff(diff: WorkerDiff, query: ResourceQuery) -> Value {
  let start = query.after_seq.unwrap_or(0) as usize;
  let files: Vec<WorkerDiffFile> = diff
    .files
    .into_iter()
    .skip(start)
    .take(query.limit)
    .collect();
  let next_after_seq = if files.len() == query.limit {
    Some(start + files.len())
  } else {
    None
  };
  json!({"files": files, "next_after_seq": next_after_seq})
}

fn write_server_response<W: Write>(
  writer: &mut W,
  response: &JsonRpcResponse,
) -> Result<(), WorkerServerError> {
  serde_json::to_writer(&mut *writer, response).map_err(WorkerServerError::Json)?;
  writer.write_all(b"\n")?;
  writer.flush()?;
  Ok(())
}

#[derive(Debug)]
pub enum WorkerServerError {
  Io(std::io::Error),
  Json(serde_json::Error),
  RequestTooLarge,
}

impl fmt::Display for WorkerServerError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Io(error) => write!(f, "worker I/O error: {error}"),
      Self::Json(error) => write!(f, "worker JSON error: {error}"),
      Self::RequestTooLarge => f.write_str("worker request exceeds the 1 MiB limit"),
    }
  }
}

impl std::error::Error for WorkerServerError {}
impl From<std::io::Error> for WorkerServerError {
  fn from(error: std::io::Error) -> Self {
    Self::Io(error)
  }
}

impl From<serde_json::Error> for WorkerServerError {
  fn from(error: serde_json::Error) -> Self {
    Self::Json(error)
  }
}

impl std::str::FromStr for WorkerCompactMode {
  type Err = String;

  fn from_str(value: &str) -> Result<Self, Self::Err> {
    match value {
      "" | "conversation" => Ok(Self::Conversation),
      "phase" => Ok(Self::Phase),
      "checkpoint" => Ok(Self::Checkpoint),
      other => Err(format!("unsupported compaction mode {other}")),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::protocol::{ClientCapabilities, ClientInfo};
  use std::sync::atomic::AtomicU64;

  #[derive(Debug)]
  struct ScriptedEngine {
    delay_ms: u64,
    calls: AtomicU64,
  }

  impl WorkerEngine for ScriptedEngine {
    fn run(
      &self,
      request: WorkerRunRequest,
      cancel: WorkerCancelToken,
    ) -> Result<WorkerExecution, WorkerError> {
      let call = self.calls.fetch_add(1, Ordering::Relaxed);
      let deadline = Instant::now() + Duration::from_millis(self.delay_ms);
      while Instant::now() < deadline {
        if cancel.is_cancelled() {
          return Ok(WorkerExecution::default());
        }
        thread::sleep(Duration::from_millis(2));
      }
      let seq = request.prior_messages.len() as u64 + 1;
      let epoch = if call == 0 { 0 } else { 1 };
      let model = ModelRef::new("fixture", if epoch == 0 { "worker" } else { "backup" });
      Ok(WorkerExecution {
        summary: Some(WorkerSummary {
          text: format!("external summary: {}", request.prompt),
          source: WorkerSummarySource::Declared,
          updated_at_ms: now_millis(),
        }),
        messages: vec![WorkerMessage {
          role: "assistant".into(),
          text: request.prompt,
          epoch,
          model: model.clone(),
          seq: Some(seq),
          external_context: None,
        }],
        trace: vec![WorkerTraceEntry {
          seq,
          kind: "turn_completed".into(),
          timestamp_ms: now_millis(),
          epoch: Some(epoch),
          model: Some(model.clone()),
          provenance: Some("runtime".into()),
        }],
        epochs: vec![WorkerModelEpoch {
          epoch,
          model: model.clone(),
          reason: if epoch == 0 { "initial" } else { "failover" }.into(),
        }],
        failovers: if epoch == 0 {
          Vec::new()
        } else {
          vec![WorkerFailover {
            from_epoch: 0,
            to_epoch: epoch,
            reason: "fixture availability failure".into(),
          }]
        },
        active_model: Some(model),
        ..WorkerExecution::default()
      })
    }
  }

  fn service(delay_ms: u64) -> WorkerService<ScriptedEngine> {
    WorkerService::new(ScriptedEngine {
      delay_ms,
      calls: AtomicU64::new(0),
    })
  }

  #[test]
  fn start_returns_handle_and_resources_are_not_terminal_output() {
    let service = service(0);
    let handle = service
      .start(StartRequest {
        prompt: "hello".into(),
        wait_ms: Some(100),
      })
      .unwrap();
    assert_eq!(handle.status, WorkerStatus::Completed);
    let summary = service
      .resource_value(&format!("session://{}/summary", handle.session_id))
      .unwrap();
    assert!(
      summary["summary"]["text"]
        .as_str()
        .unwrap()
        .contains("external summary")
    );
    assert!(
      summary["summary"]["text"]
        .as_str()
        .unwrap()
        .contains("hello")
    );
    let trace = service
      .resource_value(&format!("session://{}/trace", handle.session_id))
      .unwrap();
    assert_eq!(trace["items"][0]["kind"], "turn_completed");
  }

  #[test]
  fn long_running_runs_can_be_cancelled_and_polled() {
    let service = service(60);
    let handle = service
      .start(StartRequest {
        prompt: "long".into(),
        wait_ms: None,
      })
      .unwrap();
    assert!(matches!(
      handle.status,
      WorkerStatus::Running | WorkerStatus::Cancelling
    ));
    let result = service
      .cancel(CancelRequest {
        session_id: handle.session_id.clone(),
        run_id: handle.run_id.clone(),
        reason: Some("test".into()),
      })
      .unwrap();
    assert_eq!(result.status, WorkerStatus::Cancelling);
    thread::sleep(Duration::from_millis(20));
    let state = service
      .resource_value(&format!("session://{}/state", handle.session_id))
      .unwrap();
    assert_eq!(state["status"], "cancelled");
  }

  #[test]
  fn continuation_merges_epoch_and_failover_history() {
    let service = service(0);
    let first = service
      .start(StartRequest {
        prompt: "first".into(),
        wait_ms: Some(100),
      })
      .unwrap();
    service
      .continue_session(ContinueRequest {
        session_id: first.session_id.clone(),
        prompt: "after failover".into(),
        wait_ms: Some(100),
      })
      .unwrap();
    let state = service
      .resource_value(&format!("session://{}/state", first.session_id))
      .unwrap();
    assert_eq!(state["epochs"].as_array().unwrap().len(), 2);
    assert_eq!(state["failovers"].as_array().unwrap().len(), 1);
    assert!(state["failed_over"].as_bool().unwrap());
  }

  #[test]
  fn checkpoint_branch_is_explicitly_deferred_until_historical_snapshots_exist() {
    let service = service(0);
    let first = service
      .start(StartRequest {
        prompt: "first".into(),
        wait_ms: Some(100),
      })
      .unwrap();
    let error = service
      .branch(BranchRequest {
        session_id: first.session_id,
        from_kind: Some("checkpoint".into()),
        prompt: None,
        wait_ms: None,
      })
      .unwrap_err();
    assert_eq!(error.code, "unsupported");
  }

  #[test]
  fn branch_and_compact_preserve_epochs_without_merging_summary_into_trace() {
    let service = service(0);
    let first = service
      .start(StartRequest {
        prompt: "first".into(),
        wait_ms: Some(100),
      })
      .unwrap();
    let branch = service
      .branch(BranchRequest {
        session_id: first.session_id,
        from_kind: Some("latest".into()),
        prompt: None,
        wait_ms: None,
      })
      .unwrap();
    let compacted = service
      .compact(CompactRequest {
        session_id: branch.session_id.clone(),
        mode: "conversation".into(),
        phase: None,
        summary: Some("declared compact summary".into()),
        force: false,
      })
      .unwrap();
    assert_eq!(compacted.context_epoch, 1);
    let state = service
      .resource_value(&format!("session://{}/state", branch.session_id))
      .unwrap();
    assert_eq!(state["epochs"][0]["reason"], "initial");
    let trace = service
      .resource_value(&format!("session://{}/trace", branch.session_id))
      .unwrap();
    assert!(!trace.to_string().contains("declared compact summary"));
  }

  #[test]
  fn server_supports_handshake_tools_resources_and_notifications() {
    let service = service(0);
    let server = WorkerMcpServer::new(service);
    let init = server.handle(JsonRpcRequest::new(
      1,
      "initialize",
      Some(json!({
        "protocolVersion": "2024-11-05",
        "capabilities": ClientCapabilities::default(),
        "clientInfo": ClientInfo { name: "test".into(), version: "1".into() },
      })),
    ));
    assert!(init.error.is_none());
    let tools = server.handle(JsonRpcRequest::new(2, "tools/list", None));
    assert_eq!(tools.result.unwrap()["tools"].as_array().unwrap().len(), 5);
    let response = server.handle(JsonRpcRequest::new(
      3,
      "tools/call",
      Some(json!({"name": "agent.start", "arguments": {"prompt": "hello", "wait_ms": 100}})),
    ));
    assert!(response.error.is_none());
    let call: CallToolResult = serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(call.is_error, Some(false));
    let unknown = server.handle(JsonRpcRequest::new(4, "unknown", None));
    assert_eq!(unknown.error.unwrap().code, -32601);
  }

  #[test]
  fn stdio_server_rejects_overlong_lines_before_growing_the_buffer() {
    let server = WorkerMcpServer::new(service(0));
    let input = vec![b'x'; MAX_REQUEST_BYTES + 1];
    let mut output = Vec::new();
    assert!(matches!(
      serve_stdio(&server, input.as_slice(), &mut output),
      Err(WorkerServerError::RequestTooLarge)
    ));
  }

  #[test]
  fn stdio_server_ignores_notifications_and_reports_parse_errors() {
    let server = WorkerMcpServer::new(service(0));
    let input = format!(
      "{}\nnot-json\n{}\n",
      json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
      serde_json::to_string(&JsonRpcRequest::new(1, "resources/list", None)).unwrap()
    );
    let mut output = Vec::new();
    serve_stdio(&server, input.as_bytes(), &mut output).unwrap();
    let responses: Vec<Value> = String::from_utf8(output)
      .unwrap()
      .lines()
      .map(|line| serde_json::from_str(line).unwrap())
      .collect();
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0]["error"]["code"], -32700);
    assert!(responses[1]["result"]["resources"].is_array());
  }
}
