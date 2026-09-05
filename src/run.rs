use std::{
  fs,
  io::{self, Write},
};

use pi_rs_core::{
  AgentEvent, AttributedMessage, CancelToken, EventEnvelope, ModelProvider, ReasoningProvenance,
  RuntimeConfig, SessionEndReason, SessionHeader, SessionId, SinkError, TraceId, now_millis,
};
use pi_rs_provider::{OpenAiCompat, ProviderConfig};
use pi_rs_runtime::{StoreTrace, Trace, TurnError, TurnLoop, TurnProgress};
use pi_rs_store::{Store, WritePolicy};
use pi_rs_tools::{Executed, ToolRegistry, Workspace};

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

  let mut trace = ReportingTrace::new(StoreTrace::new(session));
  let mut progress = CliProgress;
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
      Ok(_) => runtime.end_session(SessionEndReason::UserExit),
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
  result.map_err(|error| turn_error(&error))
}

fn turn_error(error: &TurnError) -> String {
  match error {
    TurnError::Unavailable(failure) => format!("provider failure: {}", failure.message),
    TurnError::Aborted(status) => format!("turn aborted: {status:?}"),
    TurnError::Sink(message) => format!("durable sink failure: {message}"),
  }
}

struct CliProgress;

impl TurnProgress for CliProgress {
  fn on_reasoning(&mut self, text: &str, provenance: ReasoningProvenance) {
    let _ = writeln!(io::stderr(), "[{}] {text}", provenance.label());
  }

  fn on_text_delta(&mut self, text: &str) {
    let mut stdout = io::stdout().lock();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
  }

  fn on_tool_requested(&mut self, call: &pi_rs_core::ToolCallBlock) {
    let _ = writeln!(
      io::stderr(),
      "[tool requested] {} {}",
      call.name,
      call.arguments
    );
  }

  fn on_tool_progress(&mut self, call: &pi_rs_core::ToolCallBlock, text: &str) {
    let _ = writeln!(io::stderr(), "[tool output] {} {text}", call.name);
  }

  fn on_tool_finished(&mut self, call: &pi_rs_core::ToolCallBlock, executed: &Executed) {
    let _ = writeln!(
      io::stderr(),
      "[tool finished] {} {:?}",
      call.name,
      executed.state
    );
  }
}

struct ReportingTrace {
  inner: StoreTrace,
}

impl ReportingTrace {
  fn new(inner: StoreTrace) -> Self {
    Self { inner }
  }
}

impl Trace for ReportingTrace {
  fn emit(&mut self, envelope: &mut EventEnvelope) -> Result<(), SinkError> {
    if let AgentEvent::Diagnostic(diagnostic) = &envelope.event {
      let _ = writeln!(
        io::stderr(),
        "[diagnostic {:?}] {}",
        diagnostic.level,
        diagnostic.message
      );
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
