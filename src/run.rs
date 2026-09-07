use std::{
  fs,
  io::{self, Stderr, Stdout, Write},
};

use pi_rs_core::{
  AttributedMessage, CancelToken, EventEnvelope, ModelProvider, ModelRef, ReasoningProvenance,
  RuntimeConfig, SessionEndReason, SessionHeader, SessionId, SinkError, TraceId, now_millis,
};
use pi_rs_provider::{Deferred, OpenAiCompat, ProviderConfig};
use pi_rs_runtime::{StoreTrace, Trace, TurnError, TurnLoop, TurnProgress};
use pi_rs_store::{Store, WritePolicy};
use pi_rs_tools::{Executed, ToolRegistry, Workspace};
use pi_rs_tui::{Palette, Surface, TranscriptOptions, is_streamed, render_event, term};

use crate::cli::SurfaceArgs;

use crate::cli::RunArgs;

pub fn execute(args: RunArgs) -> Result<(), String> {
  // A one-shot run is a session that holds exactly one turn. It is built on the
  // same handle an interactive loop reuses for many turns, so the composition is
  // written once, in `open_session`.
  open_session(&args, |session| match session.turn(&args.prompt) {
    Ok(()) => session.close().map_err(session_error),
    Err(error) => Err(turn_error(&session.close_after_failure(error))),
  })
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
fn open_session(
  args: &RunArgs,
  turns: impl FnOnce(&mut SessionHandle<'_>) -> Result<(), String>,
) -> Result<(), String> {
  let config_text = fs::read_to_string(&args.config)
    .map_err(|error| format!("cannot read config '{}': {error}", args.config.display()))?;
  let config =
    RuntimeConfig::parse(&config_text).map_err(|error| format!("invalid config: {error}"))?;
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

  let workspace = Workspace::new(&args.cwd)
    .map_err(|error| format!("invalid workspace '{}': {error}", args.cwd.display()))?
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
  let write_policy = WritePolicy::from_retention(&config.trace, &config.redaction);
  let store = Store::open(&config.state_dir, write_policy)
    .map_err(|error| format!("cannot open durable state: {error}"))?;
  store
    .apply_retention(&config.trace, now_millis(), 1)
    .map_err(|error| format!("cannot apply trace retention: {error}"))?;
  let session_id = SessionId::new();
  let session = store
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
    .map_err(|error| format!("cannot start durable session: {error}"))?;

  let options = surface_options(&args.surface);
  let mut trace = ReportingTrace::new(StoreTrace::new(session), options);
  let progress = CliProgress::new(&tools, options);
  let mut runtime = TurnLoop::new(
    &provider,
    &tools,
    &policy,
    &mut trace,
    session_id,
    TraceId::new(),
  )
  .with_working_dir(canonical_cwd)
  .with_thinking(config.thinking);
  if let Some(backup) = &backup {
    // Failover is off until a backup exists. Attaching one is the whole
    // configuration surface: the policy comes from the primary's own capabilities.
    runtime = runtime.with_backup(backup);
  }
  let mut session = SessionHandle {
    runtime,
    progress,
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
  /// Run one user turn.
  ///
  /// The cancellation token is created here, per turn, exactly as a one-shot run
  /// creates one: a turn carries its own cancellation, and nothing shares it.
  pub fn turn(&mut self, prompt: &str) -> Result<(), TurnError> {
    self
      .runtime
      .run_turn(prompt, &CancelToken::new(), &mut self.progress)
      .map(|_| ())
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

fn session_error(error: SessionError) -> String {
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

  fn flush(&mut self) -> Result<(), SinkError> {
    self.inner.flush()
  }
}
