use std::{
  fs,
  io::{self, Stderr, Stdout, Write},
  path::Path,
};

use pi_rs_core::{
  AttributedMessage, CancelToken, CheckpointId, ContextCapsule, EventEnvelope, Message,
  ModelProvider, ModelRef, ReasoningProvenance, RuntimeConfig, SessionEndReason, SessionHeader,
  SessionId, SinkError, TraceId, TurnId, now_millis,
};
use pi_rs_provider::{Deferred, OpenAiCompat, ProviderConfig};
use pi_rs_runtime::{StoreTrace, Trace, TurnError, TurnLoop, TurnProgress, TurnReport};
use pi_rs_store::{Store, WritePolicy};
use pi_rs_tools::{Executed, ToolRegistry, Workspace};
use pi_rs_tui::{Palette, Surface, TranscriptOptions, is_streamed, render_event, term};

use crate::cli::SurfaceArgs;

use crate::cli::RunArgs;

pub fn execute(args: RunArgs) -> Result<(), String> {
  // A one-shot run is a session that holds exactly one turn. It is built on the
  // same handle an interactive loop reuses for many turns, so the composition is
  // written once, in `open_session`.
  open_session(
    &args.config,
    &args.cwd,
    &args.surface,
    args.resume.as_deref(),
    |session| match session.turn(&args.prompt) {
      Ok(()) => session.close().map_err(session_error),
      Err(error) => Err(turn_error(&session.close_after_failure(error))),
    },
  )
}

/// Open one durable session and hand it to `turns`, which may run as many turns as
/// the caller wants before closing it.
///
/// The composition lives here alone, and its order is the contract: every fallible
/// input and composition check precedes the first `run_turn`, the only operation
/// that can contact a provider, so a bad config never spends a turn.
///
/// The handle borrows these parts rather than owning them because `TurnLoop` holds
/// `&mut dyn Trace`: a handle that owned both would be self-referential. Scoping
/// the borrow to this call is what keeps it sound, and it is why a caller closes
/// from inside `turns` rather than after this function returns.
pub(crate) fn open_session(
  config: &Path,
  cwd: &Path,
  surface: &SurfaceArgs,
  // A session to continue rather than begin, as written by the caller; `None` opens
  // a new session. Resolution against the store happens here, not in parsing.
  resume: Option<&str>,
  turns: impl FnOnce(&mut SessionHandle<'_>) -> Result<(), String>,
) -> Result<(), String> {
  let config_text = fs::read_to_string(config)
    .map_err(|error| format!("cannot read config '{}': {error}", config.display()))?;
  let config =
    RuntimeConfig::parse(&config_text).map_err(|error| format!("invalid config: {error}"))?;
  // RKB normalization only enriches the cloned manager configuration with the
  // provider's read-only retrieval tool names. No MCP process is started here.
  let mcp_servers = pi_rs_rkb::RkbSetup::normalize_configs(&config.mcp_servers);
  let rkb_setup = pi_rs_rkb::RkbSetup::discover(&mcp_servers);
  let endpoint = config.endpoint_for(&config.primary).ok_or_else(|| {
    format!(
      "invalid config: primary model {} has no endpoint entry",
      config.primary
    )
  })?;
  let provider_config = ProviderConfig::from_endpoint(endpoint)
    .map_err(|error| format!("invalid primary endpoint: {error}"))?;
  let provider = OpenAiCompat::new(provider_config)
    .map_err(|error| format!("invalid primary endpoint: {error}"))?;
  // A configured backup is attached as a handle only. Building its adapter here
  // would install a TLS agent and read a credential variable for a model that the
  // overwhelming majority of sessions never need.
  let backup = backup_provider(&config)?;

  let workspace = Workspace::new(cwd)
    .map_err(|error| format!("invalid workspace '{}': {error}", cwd.display()))?
    .with_read_outside(false);
  let canonical_cwd = workspace
    .root()
    .to_str()
    .ok_or_else(|| "invalid workspace: canonical path is not valid UTF-8".to_string())?
    .to_string();
  let mut tool_policy = config.tools.clone();
  // The command-line workspace is the explicit authority for this invocation;
  // policy application must not recreate a second, more permissive workspace.
  tool_policy.cwd = None;
  let tools = ToolRegistry::new(workspace)
    .with_policy(&tool_policy)
    .with_builtins();
  // Skills are offered the way Pi offers them: a control prompt in front of every
  // request, naming what exists and telling the model to read the file. Only the
  // global locations are read — a skill is instructions for the model, and this
  // command has no trust decision to consult about the workspace, so the project's
  // own skill files stay unread (and `pi-rs skills --project` stays how one is seen).
  // The scan is two small directories, which is what lets it sit on the startup path.
  let mut skills_prompt =
    pi_rs_compat::skill::discover(&pi_rs_compat::scan::Discovery::new(canonical_cwd.clone()))
      .control_prompt();
  // RKB setup is config discovery only: the MCP process remains disconnected until
  // the caller explicitly enables the discovered server. When configured, offer the
  // first-party skill inline so a packaged binary does not need a source-tree path
  // merely to explain citation and rehydration rules.
  if let Some(setup) = &rkb_setup {
    if !skills_prompt.is_empty() {
      skills_prompt.push_str("\n\n");
    }
    skills_prompt.push_str(setup.skill());
  }

  let policy: Box<dyn pi_rs_core::context::ContextPolicy> =
    if config.adaptive_context.unwrap_or(false) {
      Box::new(pi_rs_experiments::AdaptiveContextPolicy::new(
        config.context_profile,
        provider.capabilities().context_window,
        true,
      ))
    } else {
      let mut policy = pi_rs_core::ProfilePolicy::new(
        config.context_profile,
        provider.capabilities().context_window,
      );
      if let Some(overrides) = &config.context_overrides {
        if let Some(value) = overrides.warn_tokens {
          policy.thresholds.warn_tokens = value;
        }
        if let Some(value) = overrides.reduce_tokens {
          policy.thresholds.reduce_tokens = value;
        }
        if let Some(value) = overrides.compact_tokens {
          policy.thresholds.compact_tokens = value;
        }
        if let Some(value) = overrides.checkpoint_tokens {
          policy.thresholds.checkpoint_tokens = value;
        }
        if let Some(value) = overrides.recent_target_tokens {
          policy.thresholds.recent_target_tokens = value;
        }
      }
      Box::new(policy)
    };
  let write_policy = WritePolicy::from_retention(&config.trace, &config.redaction);
  // The name is resolved against a read-only store, before `Store::open`, because `open`
  // creates the state layout: an id the store does not hold must leave the store exactly
  // as it was found, with no session written and no provider contacted. It is resolved by
  // the trace command's own rule, so a prefix cannot name two things and the two commands
  // cannot disagree about what one session id means.
  let continuing = match resume {
    Some(wanted) => {
      let read_only = Store::new(&config.state_dir, write_policy.clone());
      Some(crate::trace::resolve_session(&read_only, Some(wanted))?)
    }
    None => None,
  };
  let store = Store::open(&config.state_dir, write_policy)
    .map_err(|error| format!("cannot open durable state: {error}"))?;
  // Retention sheds history and protects only the newest sessions, so a pass would delete
  // the older session `--resume` was asked to continue. Shedding is what starting a new
  // session is for, and nothing else.
  if continuing.is_none() {
    store
      .apply_retention(&config.trace, now_millis(), 1)
      .map_err(|error| format!("cannot apply trace retention: {error}"))?;
  }
  let context = match &continuing {
    Some(session_id) => continue_context(&store, session_id)?,
    None => Vec::new(),
  };
  let session_id = continuing.clone().unwrap_or_else(SessionId::new);
  let session = match &continuing {
    // The existing log is reopened and appended to: a continuation is one session file,
    // and a second file under a new id is not a continuation of anything.
    Some(session_id) => store
      .resume(session_id)
      .map_err(|error| format!("cannot continue session {}: {error}", session_id.as_str()))?,
    None => store
      .begin(SessionHeader {
        session_id: session_id.clone(),
        version: pi_rs_core::session::SESSION_SCHEMA_VERSION,
        started_at_ms: now_millis(),
        working_dir: canonical_cwd.clone(),
        model: provider.model().clone(),
        parent_session: None,
        branched_from_event: None,
        imported_from: None,
      })
      .map_err(|error| format!("cannot start durable session: {error}"))?,
  };

  let options = surface_options(surface);
  let mut trace = ReportingTrace::new(StoreTrace::new(session), options);
  let progress = CliProgress::new(&tools, options);
  let mut runtime = TurnLoop::new(
    &provider,
    &tools,
    policy.as_ref(),
    &mut trace,
    session_id,
    TraceId::new(),
  )
  // Empty for a session that has just begun, so this only ever carries a resumed one.
  .with_messages(context)
  .with_working_dir(canonical_cwd)
  .with_thinking(config.thinking)
  .with_compaction_strategy(pi_rs_runtime::CompactionStrategy::Summarize);
  if !skills_prompt.is_empty() {
    // The skill-control prompt is the whole system prompt pi-rs speaks today, and
    // with_system stays unset when there is nothing to offer.
    runtime = runtime.with_system(skills_prompt);
  }
  if let Some(backup) = &backup {
    // Failover is off until a backup exists. Attaching one is the whole
    // configuration surface: the policy comes from the primary's own capabilities.
    runtime = runtime.with_backup(backup);
  }
  let mut mcp_manager = pi_rs_mcp::McpManager::new(mcp_servers);
  let mut session = SessionHandle {
    runtime,
    progress,
    tools: &tools,
    mcp_manager: &mut mcp_manager,
    transcript_error: None,
  };
  turns(&mut session)
}

/// One session, open for as many turns as the caller wants.
///
/// The runtime is long-lived on purpose, and the handle is what keeps it that way:
/// the memory between turns is the history `TurnLoop` already owns, so a caller
/// that rebuilt a loop per turn would silently discard that history and re-emit
/// `SessionStarted`. Built by [`open_session`], which owns the providers, tools,
/// context policy, and sink the handle borrows.
pub struct SessionHandle<'a> {
  runtime: TurnLoop<'a>,
  progress: CliProgress<'a>,
  tools: &'a ToolRegistry,
  mcp_manager: &'a mut pi_rs_mcp::McpManager,
  /// A transcript write failure must not decide whether the session closes: the
  /// durable record is the product, so the failure is carried out and reported
  /// after the session has ended.
  transcript_error: Option<io::Error>,
}

// The turn error carries the full normalized failure the caller has to print, and
// a session error is either that or one I/O error; boxing either would only move
// the allocation to the path that reports a failure. Same allowance the runtime
// crate makes for `TurnError`.
#[allow(clippy::result_large_err)]
impl SessionHandle<'_> {
  /// Which model will answer the next request.
  ///
  /// Asked of the runtime rather than read from the config once: a failover changes
  /// the answer mid-session, and a surface that named the model configured at the
  /// start would be quietly wrong from that moment on.
  pub fn model(&self) -> ModelRef {
    self.runtime.active_model()
  }

  /// Run one user turn that nothing outside this call can cancel.
  pub fn turn(&mut self, prompt: &str) -> Result<(), TurnError> {
    self.turn_with(prompt, &CancelToken::new()).map(|_| ())
  }

  /// Run one user turn under a cancellation token the caller holds.
  ///
  /// A turn carries its own cancellation, and a token is one-shot — nothing ever
  /// clears it — so a caller that can interrupt more than one turn supplies a fresh
  /// token each turn rather than reusing the one it interrupted.
  ///
  /// A canceled turn is a report, not an error: the user asked for the turn to stop,
  /// which is a terminal state rather than a fault, and the runtime has already
  /// recorded it as one. `status` is therefore the only place a caller can tell
  /// `Cancelled` from `Completed`, and it never has to infer either from whether
  /// output happened to arrive.
  pub fn turn_with(&mut self, prompt: &str, cancel: &CancelToken) -> Result<TurnReport, TurnError> {
    self.runtime.run_turn(prompt, cancel, &mut self.progress)
  }

  /// Run one user turn with external context folded into the message path and recorded
  /// in the event trace.
  #[allow(dead_code)]
  pub fn turn_with_external_context(
    &mut self,
    prompt: &str,
    external_context: &[pi_rs_core::ExternalContextItem],
    cancel: &CancelToken,
  ) -> Result<TurnReport, TurnError> {
    self.runtime.run_turn_with_external_context(
      prompt,
      external_context,
      cancel,
      &mut self.progress,
    )
  }

  /// Compact earlier conversation history into a durable summary epoch.
  pub fn compact(&mut self, summary: Option<&str>) -> Result<u32, TurnError> {
    let turn_id = TurnId::new();
    let target_tokens = 4_096;
    self
      .runtime
      .compact_with_summary_or(&turn_id, target_tokens, summary)
  }

  /// Perform L2 semantic phase compaction across a task boundary.
  pub fn compact_phase(
    &mut self,
    phase: &str,
    summary: Option<&str>,
    force: bool,
  ) -> Result<u32, TurnError> {
    let turn_id = TurnId::new();
    self.runtime.compact_phase(&turn_id, phase, summary, force)
  }

  /// Return current statuses of all configured MCP servers.
  pub fn mcp_statuses(&self) -> Vec<pi_rs_mcp::McpServerStatus> {
    self.mcp_manager.statuses()
  }

  /// Enable and connect a configured MCP server, registering its tools into the session.
  pub fn mcp_enable(&mut self, name: &str) -> Result<usize, pi_rs_mcp::McpError> {
    let tools = self.mcp_manager.enable_server(name)?;
    let count = tools.len();
    for tool in tools {
      self.tools.register_shared(Box::new(tool));
    }
    Ok(count)
  }

  /// Disable and disconnect a configured MCP server, removing its tools from the session.
  pub fn mcp_disable(&mut self, name: &str) -> Result<usize, pi_rs_mcp::McpError> {
    self.mcp_manager.disable_server(name)?;
    let prefix = format!("mcp__{name}__");
    Ok(self.tools.unregister_prefix(&prefix))
  }

  /// Create a checkpoint capsule, append its barrier to the session log, emit
  /// `CheckpointCreated`, and reset visible messages.
  #[allow(dead_code)]
  pub fn checkpoint(
    &mut self,
    capsule: Option<pi_rs_core::ContextCapsule>,
  ) -> Result<pi_rs_core::CheckpointCreated, TurnError> {
    let turn_id = TurnId::new();
    let capsule = match capsule {
      Some(c) => c,
      None => {
        let state = pi_rs_core::ContextState::zero(64_000);
        self.runtime.synthesize_capsule(&state, "manual checkpoint")
      }
    };
    self.runtime.checkpoint(&turn_id, capsule)
  }

  /// Session identifier for this active session.
  #[allow(dead_code)]
  pub fn session_id(&self) -> &SessionId {
    self.runtime.session_id()
  }

  /// List all checkpoint capsules recorded for this session.
  pub fn list_checkpoints(
    &self,
  ) -> Result<Vec<(pi_rs_core::CheckpointId, pi_rs_core::ContextCapsule)>, String> {
    self
      .runtime
      .list_checkpoints()
      .map_err(|e| format!("cannot list checkpoints: {e:?}"))
  }

  /// The model currently active for generation.
  #[allow(dead_code)]
  pub fn active_model(&self) -> pi_rs_core::ModelRef {
    self.runtime.active_model()
  }

  /// The backup model, if configured.
  #[allow(dead_code)]
  pub fn backup_model(&self) -> Option<pi_rs_core::ModelRef> {
    self.runtime.backup_model()
  }

  /// The primary model.
  #[allow(dead_code)]
  pub fn primary_model(&self) -> pi_rs_core::ModelRef {
    self.runtime.primary_model()
  }

  /// `true` if the session is currently generating with the backup model.
  #[allow(dead_code)]
  pub fn failed_over(&self) -> bool {
    self.runtime.failed_over()
  }

  /// Manually switch active generation to the backup model.
  pub fn failover_manual(&mut self) -> Result<pi_rs_core::ModelEpoch, String> {
    self.runtime.failover_manual().map_err(|e| format!("{e:?}"))
  }

  /// Manually switch active generation back to the primary model.
  pub fn switch_back_manual(&mut self) -> Result<pi_rs_core::ModelEpoch, String> {
    self
      .runtime
      .switch_back_manual()
      .map_err(|e| format!("{e:?}"))
  }

  /// Flush the transcript, end the session as a user exit, and report what the
  /// caller should show.
  ///
  /// The flush precedes the end event: the surface may hold an unterminated
  /// reasoning line, and the durable `SessionEnded` event renders underneath it. A
  /// write failure is reported only once the session is durably closed, and a sink
  /// failure outranks it, because a lost line is not worth losing the record.
  pub fn close(&mut self) -> Result<(), SessionError> {
    self.transcript_error = self.progress.finish().err();
    self
      .runtime
      .end_session(SessionEndReason::UserExit)
      .map_err(SessionError::Turn)?;
    match self.transcript_error.take() {
      Some(error) => Err(SessionError::Transcript(error)),
      None => Ok(()),
    }
  }

  /// End a session whose turn already failed, and return the error to report.
  ///
  /// A recoverable failure still gets a durable end event that carries what
  /// happened; a sink failure while writing that event is the more urgent fact and
  /// replaces it. The transcript is not flushed here — only a completed turn gets a
  /// summary line.
  pub fn close_after_failure(&mut self, error: TurnError) -> TurnError {
    if error.session_recoverable() {
      if let Err(sink_error) = self.runtime.end_session(SessionEndReason::Fatal {
        message: turn_error(&error),
      }) {
        return sink_error;
      }
    }
    error
  }
}

/// Why a session did not end quietly.
pub enum SessionError {
  /// The turn failed, or the durable sink could not record it.
  Turn(TurnError),
  /// The transcript could not be written; the durable record is intact.
  Transcript(io::Error),
}

pub(crate) fn session_error(error: SessionError) -> String {
  match error {
    SessionError::Turn(error) => turn_error(&error),
    SessionError::Transcript(error) => format!("cannot write transcript: {error}"),
  }
}

/// Resolve the surface from arguments and the environment.
///
/// Colour and width answer two different questions, so a run with `2> log` gets a
/// wide, colourless transcript and (if stdout is a terminal) a coloured answer. An
/// explicit `--width` is respected even on a pipe, which is how a user narrows a
/// log deliberately rather than by accident.
/// The configured backup as a provider that does not exist yet.
///
/// The adapter is built the first time the runtime actually addresses a request to
/// it, which is also when its credential is resolved from the environment. What the
/// failover gate needs beforehand — the model reference and the capability
/// declaration — is read from config, so a backup that is never needed costs
/// nothing at startup and never touches a credential it does not use.
/// The model-visible context a resumed session is asked to continue from.
///
/// This is the store's checkpoint path, not a full hydration: where a checkpoint barrier
/// exists the records before it are already summarized inside the capsule, so the
/// post-barrier window is both the cheaper and the honest reading of what a live session
/// would have held.
///
/// A context that cannot be rebuilt is refused here rather than answered with a shorter
/// conversation. Silently dropping the part that is missing is how a continuation becomes
/// a fabrication: the user asked to continue a session, so the answer has to say which
/// part of it could not be recovered.
fn continue_context(store: &Store, session_id: &SessionId) -> Result<Vec<Message>, String> {
  let restored = store
    .restore(session_id)
    .map_err(|error| format!("cannot continue session {session_id}: {error}"))?;
  if restored.malformed_records > 0 {
    return Err(format!(
      "cannot continue session {}: {} record(s) of its session log are unreadable, so the \
       earlier turns it would continue from are missing",
      session_id.as_str(),
      restored.malformed_records
    ));
  }
  let mut messages = Vec::new();
  if let Some(capsule) = restored.checkpoint {
    messages.push(Message::user(capsule.format_for_model()));
  }
  messages.extend(restored.messages.into_iter().map(|message| message.message));
  Ok(messages)
}

fn backup_provider(config: &RuntimeConfig) -> Result<Option<Deferred>, String> {
  let Some(model) = config.backup.clone() else {
    return Ok(None);
  };
  // `RuntimeConfig::validate` already requires every usable model to have an
  // endpoint once endpoints are declared at all; the case left over is a hand-run
  // primary with no endpoint table, which cannot address a backup either.
  let endpoint = config
    .endpoint_for(&model)
    .ok_or_else(|| format!("invalid config: backup model {model} has no endpoint entry"))?;
  let declared = endpoint.capabilities.clone();
  let endpoint = endpoint.clone();
  Ok(
    Deferred::new(model, declared, move || {
      let config = ProviderConfig::from_endpoint(&endpoint).map_err(|error| error.to_string())?;
      OpenAiCompat::new(config)
        .map(|provider| Box::new(provider) as Box<dyn ModelProvider>)
        .map_err(|error| error.to_string())
    })
    .into(),
  )
}

fn surface_options(args: &SurfaceArgs) -> TranscriptOptions {
  let stderr = term::Stream::Stderr;
  TranscriptOptions {
    palette: if args.color.resolve(stderr.is_terminal()) {
      Palette::colored()
    } else {
      Palette::monochrome()
    },
    width: args.width.unwrap_or_else(|| stderr.width().unwrap_or(0)),
    show_reasoning: args.reasoning,
    diagnostics: args.diagnostics,
    ..TranscriptOptions::default()
  }
}

fn turn_error(error: &TurnError) -> String {
  match error {
    TurnError::Unavailable(failure) => format!("provider failure: {}", failure.message),
    TurnError::Aborted(status) => format!("turn aborted: {status:?}"),
    TurnError::Sink(message) => format!("durable sink failure: {message}"),
  }
}

/// The live surface for one turn.
///
/// The surface, not a writer, owns streaming because it holds the one fact that
/// cannot be recovered from a delta alone: whether a reasoning block is still open.
/// A caller that re-derived it per callback would label every chunk as a new
/// thought, which is exactly the collapse the provenance type exists to prevent.
///
/// The tool registry is consulted for declared risk rather than guessed from the
/// tool name: `[needs check]` is a claim that the user may have work to do, and it
/// has to come from the tool's own metadata.
struct CliProgress<'a> {
  surface: Surface<Stdout, Stderr>,
  tools: &'a ToolRegistry,
  io_error: Option<io::Error>,
}

impl<'a> CliProgress<'a> {
  fn new(tools: &'a ToolRegistry, options: TranscriptOptions) -> Self {
    Self {
      surface: Surface::new(io::stdout(), io::stderr(), options),
      tools,
      io_error: None,
    }
  }

  /// Close any open block and flush both streams.
  fn finish(&mut self) -> io::Result<()> {
    self.surface.finish()?;
    match self.io_error.take() {
      Some(error) => Err(error),
      None => Ok(()),
    }
  }

  fn mutating(&mut self, name: &str) -> bool {
    self
      .tools
      .metadata_for(name)
      .is_some_and(|metadata| !metadata.read_only)
  }
}

impl TurnProgress for CliProgress<'_> {
  fn on_user_message(&mut self, text: &str) {
    save(&mut self.io_error, self.surface.user_message(text));
  }

  fn on_request_started(&mut self, model: &ModelRef) {
    save(&mut self.io_error, self.surface.request_started(model));
  }

  fn on_reasoning(&mut self, text: &str, provenance: ReasoningProvenance) {
    save(&mut self.io_error, self.surface.reasoning(text, provenance));
  }

  fn on_text_delta(&mut self, text: &str) {
    save(&mut self.io_error, self.surface.text_delta(text));
  }

  fn on_tool_requested(&mut self, call: &pi_rs_core::ToolCallBlock) {
    let mutating = self.mutating(&call.name);
    save(
      &mut self.io_error,
      self
        .surface
        .tool_requested(&call.name, &call.arguments, !mutating),
    );
  }

  fn on_tool_progress(&mut self, call: &pi_rs_core::ToolCallBlock, text: &str) {
    save(
      &mut self.io_error,
      self.surface.tool_progress(&call.name, text),
    );
  }

  fn on_tool_finished(&mut self, call: &pi_rs_core::ToolCallBlock, executed: &Executed) {
    let mutating = self.mutating(&call.name);
    save(
      &mut self.io_error,
      self.surface.tool_finished(
        &call.name,
        executed.state,
        mutating,
        executed.refusal.as_deref(),
      ),
    );
  }
}

/// Keep the first write failure instead of dropping every one of them.
///
/// Swallowing each error makes a closed pipe indistinguishable from an uneventful
/// turn; the first failure is what explains the missing output.
fn save(slot: &mut Option<io::Error>, result: io::Result<()>) {
  if let Err(error) = result
    && slot.is_none()
  {
    *slot = Some(error);
  }
}

/// The durable sink, plus transcript rendering of the events a live turn does not
/// stream.
///
/// Two writers share stderr here: this one, for durable events, and the surface, for
/// streamed content. They are partitioned rather than synchronised — [`is_streamed`]
/// says which events the surface already printed — so no event is printed twice. A
/// diagnostic can still land between two reasoning deltas; since every line the
/// surface writes is newline-terminated, the worst case is adjacency, not a torn
/// line.
struct ReportingTrace {
  inner: StoreTrace,
  options: TranscriptOptions,
}

impl ReportingTrace {
  fn new(inner: StoreTrace, options: TranscriptOptions) -> Self {
    Self { inner, options }
  }
}

impl Trace for ReportingTrace {
  fn emit(&mut self, envelope: &mut EventEnvelope) -> Result<(), SinkError> {
    if self.options.diagnostics.shows(&envelope.event) && !is_streamed(&envelope.event) {
      let palette = self.options.palette;
      for line in render_event(&envelope.event, &self.options) {
        // A transcript that cannot be written must not fail the turn: the durable
        // record is the product, and losing a line is not reason enough to lose a
        // turn.
        let _ = writeln!(io::stderr(), "{}", line.render(palette));
      }
    }
    self.inner.emit(envelope)
  }

  fn record_message(&mut self, attributed: &AttributedMessage) -> Result<(), SinkError> {
    self.inner.record_message(attributed)
  }

  fn put_payload(&mut self, bytes: &[u8]) -> Result<Option<pi_rs_core::BlobRef>, SinkError> {
    self.inner.put_payload(bytes)
  }

  fn create_checkpoint(
    &mut self,
    capsule: &ContextCapsule,
  ) -> Result<Option<(CheckpointId, String)>, SinkError> {
    self.inner.create_checkpoint(capsule)
  }

  fn list_checkpoints(&self) -> Result<Vec<(CheckpointId, ContextCapsule)>, SinkError> {
    self.inner.list_checkpoints()
  }

  fn flush(&mut self) -> Result<(), SinkError> {
    self.inner.flush()
  }
}

#[cfg(test)]
mod tests;
