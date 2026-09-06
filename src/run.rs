use std::{
  fs,
  io::{self, Stderr, Stdout, Write},
};

use pi_rs_core::{
  AttributedMessage, CancelToken, EventEnvelope, ModelProvider, ModelRef, ReasoningProvenance,
  RuntimeConfig, SessionEndReason, SessionHeader, SessionId, SinkError, TraceId, now_millis,
};
use pi_rs_provider::{OpenAiCompat, ProviderConfig};
use pi_rs_runtime::{StoreTrace, Trace, TurnError, TurnLoop, TurnProgress};
use pi_rs_store::{Store, WritePolicy};
use pi_rs_tools::{Executed, ToolRegistry, Workspace};
use pi_rs_tui::{Palette, Surface, TranscriptOptions, is_streamed, render_event, term};

use crate::cli::SurfaceArgs;

use crate::cli::RunArgs;

pub fn execute(args: RunArgs) -> Result<(), String> {
  // Every fallible input and composition check precedes `run_turn`, the only
  // operation in this command that can contact a provider.
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
  let mut progress = CliProgress::new(&tools, options);
  // A transcript write failure must not decide whether the session closes: the
  // durable record is the product, so the failure is carried out and reported after
  // the session has ended.
  let transcript_error: Option<io::Error>;
  let result = {
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
    match runtime.run_turn(&args.prompt, &CancelToken::new(), &mut progress) {
      Ok(_) => {
        // Flush before the summary line: the surface may hold an unterminated
        // reasoning line, and the durable `SessionEnded` event renders underneath it.
        transcript_error = progress.finish().err();
        runtime.end_session(SessionEndReason::UserExit)
      }
      Err(error) => {
        if error.session_recoverable() {
          runtime
            .end_session(SessionEndReason::Fatal {
              message: turn_error(&error),
            })
            .map_err(|sink_error| turn_error(&sink_error))?;
        }
        return Err(turn_error(&error));
      }
    }
  };
  result.map_err(|error| turn_error(&error))?;
  match transcript_error {
    Some(error) => Err(format!("cannot write transcript: {error}")),
    None => Ok(()),
  }
}

/// Resolve the surface from arguments and the environment.
///
/// Colour and width answer two different questions, so a run with `2> log` gets a
/// wide, colourless transcript and (if stdout is a terminal) a coloured answer. An
/// explicit `--width` is respected even on a pipe, which is how a user narrows a
/// log deliberately rather than by accident.
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
