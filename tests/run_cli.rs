//! One-shot agent runs through the real binary: what a turn prints, what it writes, and
//! what it refuses to do.
//!
//! The fake provider is shared with `tests/failover_cli.rs` and lives in
//! `tests/fake_provider`, where the reason it is deterministic is written down.

mod fake_provider;

use std::{
  fs,
  net::TcpListener,
  path::{Path, PathBuf},
  process::{Command, Output},
};

use pi_rs_core::{
  AgentEvent, ModelCapabilities, ModelEndpoint, ModelRef, ReasoningExposure, Role, RuntimeConfig,
};
use pi_rs_store::{StateLayout, Store, TraceJournal, WritePolicy};
use tempfile::TempDir;

use fake_provider::{FakeServer, sse, status_response, text_response};

/// A response that asks for a tool, optionally after some reasoning, then ends the
/// stream. The adapter joins the fragments into one call when the stream ends.
fn tool_response(id: &str, name: &str, arguments: &str, reasoning: Option<&str>) -> String {
  let mut events = Vec::new();
  if let Some(reasoning) = reasoning {
    events.push(serde_json::json!({
      "choices": [{"delta": {"reasoning_content": reasoning}}]
    }));
  }
  events.push(serde_json::json!({
    "choices": [{"delta": {"tool_calls": [{
      "index": 0,
      "id": id,
      "function": {"name": name, "arguments": arguments}
    }]}}]
  }));
  events.push(serde_json::json!({
    "choices": [{"delta": {}, "finish_reason": "tool_calls"}]
  }));
  sse(&events)
}

fn write_config(root: &Path, base_url: &str, auto_approve_mutating: bool) -> PathBuf {
  write_config_exposing(
    root,
    base_url,
    auto_approve_mutating,
    ReasoningExposure::Native,
  )
}

/// The same config, declaring something else about reasoning output.
///
/// The declaration is what decides the provenance claim attached to thinking text,
/// so a test about provenance has to say what its endpoint claims to expose.
fn write_config_exposing(
  root: &Path,
  base_url: &str,
  auto_approve_mutating: bool,
  exposed_reasoning: ReasoningExposure,
) -> PathBuf {
  let state = root.join("state");
  let mut config = RuntimeConfig::new(ModelRef::new("fake", "agent"), state.to_string_lossy());
  config.endpoints.push(ModelEndpoint {
    provider: "fake".into(),
    model: "agent".into(),
    base_url: Some(base_url.into()),
    api_key_env: None,
    api_key: None,
    capabilities: ModelCapabilities {
      text: true,
      images: false,
      tools: true,
      exposed_reasoning,
      context_window: 32_768,
      max_output_tokens: Some(1_024),
    },
    max_output_tokens: Some(1_024),
  });
  config.tools.auto_approve_mutating = auto_approve_mutating;
  let path = root.join("config.json");
  fs::write(&path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
  path
}

fn run(config: &Path, cwd: &Path, prompt: &str) -> Output {
  Command::new(env!("CARGO_BIN_EXE_pi-rs"))
    .args(["run", "--config"])
    .arg(config)
    .arg("--cwd")
    .arg(cwd)
    .args(["--prompt", prompt])
    .output()
    .expect("run pi-rs")
}

#[test]
fn one_turn_streams_and_persists_tools_messages_and_trace() {
  let temp = TempDir::new().unwrap();
  let workspace = temp.path().join("workspace");
  fs::create_dir(&workspace).unwrap();
  let server = FakeServer::answer(vec![
    tool_response(
      "call_write",
      "write",
      r#"{"path":"model.txt","contents":"from tool\n"}"#,
      Some("choose a file"),
    ),
    tool_response(
      "call_exec",
      "exec",
      r#"{"command":"printf executed > exec.txt"}"#,
      None,
    ),
    text_response("completed"),
  ]);
  let config = write_config(temp.path(), &server.base_url(), true);

  let output = run(&config, &workspace, "make the files");
  let requests = server.requests();
  assert!(
    output.status.success(),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  // The assistant answer is byte-faithful on stdout, and the surface terminates the
  // prose block so a piped answer ends with a newline instead of a shell's `%`.
  assert_eq!(String::from_utf8_lossy(&output.stdout), "completed\n");
  let stderr = String::from_utf8_lossy(&output.stderr);
  // Reasoning is labelled with its provenance, never styled as prose.
  assert!(stderr.contains("[reasoning] choose a file"), "{stderr}");
  // The request line names the operation, the argument, and the declared risk.
  assert!(stderr.contains("[tool] write"), "{stderr}");
  assert!(stderr.contains("path=model.txt"), "{stderr}");
  assert!(stderr.contains("mutating"), "{stderr}");
  // Completion is spelled out, not a debug-formatted enum variant.
  assert!(stderr.contains("[tool ok] exec · succeeded"), "{stderr}");
  assert!(!stderr.contains("Succeeded"), "{stderr}");
  // Routine chrome is calm by default: one line per model request is noise.
  assert!(!stderr.contains("[request]"), "{stderr}");
  assert!(stderr.contains("[session end]"), "{stderr}");
  assert_eq!(requests.len(), 3);
  assert_eq!(
    fs::read_to_string(workspace.join("model.txt")).unwrap(),
    "from tool\n"
  );
  assert_eq!(
    fs::read_to_string(workspace.join("exec.txt")).unwrap(),
    "executed"
  );

  let state = temp.path().join("state");
  let layout = StateLayout::new(&state);
  let session_id = layout
    .list_session_ids()
    .unwrap()
    .pop()
    .expect("session id");
  let restored = Store::new(&state, WritePolicy::default())
    .restore(&session_id)
    .unwrap();
  assert_eq!(
    restored.header.working_dir,
    workspace.canonicalize().unwrap().to_string_lossy()
  );
  assert_eq!(restored.header.model, ModelRef::new("fake", "agent"));

  let messages: Vec<_> = restored.messages.iter().collect();
  assert!(messages.iter().any(|message| message.role == Role::User));
  assert!(
    messages
      .iter()
      .any(|message| message.role == Role::Assistant)
  );
  assert!(messages.iter().any(|message| message.role == Role::Tool));
  assert!(messages.iter().all(|message| {
    message.epoch == 0 && message.model == ModelRef::new("fake", "agent") && message.seq.is_some()
  }));

  let trace = TraceJournal::read(&layout.trace_path(&session_id))
    .unwrap()
    .items;
  let sequences: Vec<u64> = trace
    .iter()
    .map(|entry| entry.envelope.meta.seq.expect("store sequence").0)
    .collect();
  assert_eq!(sequences, (1..=sequences.len() as u64).collect::<Vec<_>>());
  assert!(trace.iter().any(|entry| matches!(
    &entry.envelope.event,
    AgentEvent::SessionStarted(started)
      if started.working_dir == workspace.canonicalize().unwrap().to_string_lossy()
        && started.model == ModelRef::new("fake", "agent")
  )));
  assert!(trace.iter().any(|entry| matches!(
    &entry.envelope.event,
    AgentEvent::ReasoningDelta(delta)
      if delta.provenance == pi_rs_core::ReasoningProvenance::Native
  )));

  for call_id in ["call_write", "call_exec"] {
    let requested = trace
      .iter()
      .position(|entry| {
        matches!(
          &entry.envelope.event,
          AgentEvent::ToolRequested(event) if event.call_id.as_str() == call_id
        )
      })
      .unwrap();
    let started = trace
      .iter()
      .position(|entry| {
        matches!(
          &entry.envelope.event,
          AgentEvent::ToolStarted(event) if event.call_id.as_str() == call_id
        )
      })
      .unwrap();
    let completed = trace
      .iter()
      .position(|entry| {
        matches!(
          &entry.envelope.event,
          AgentEvent::ToolCompleted(event) if event.call_id.as_str() == call_id
        )
      })
      .unwrap();
    assert!(requested < started && started < completed);
  }
  for message in messages {
    let entry = trace
      .iter()
      .find(|entry| entry.envelope.meta.event_id == message.event_id)
      .expect("message introducing event");
    assert_eq!(entry.envelope.meta.seq, message.seq);
    assert!(
      matches!(
        (message.role, &entry.envelope.event),
        (Role::User, AgentEvent::UserMessage(_))
          | (Role::Assistant, AgentEvent::AssistantDelta(_))
          | (Role::Assistant, AgentEvent::ModelRequestCompleted(_))
          | (Role::Tool, AgentEvent::ToolCompleted(_))
          | (Role::Tool, AgentEvent::ToolFailed(_))
          | (Role::Tool, AgentEvent::ToolUnknown(_))
      ),
      "message role {:?} attributed to {:?}",
      message.role,
      entry.envelope.event
    );
  }
}

#[test]
fn mutating_tools_are_denied_without_explicit_auto_approval() {
  let temp = TempDir::new().unwrap();
  let workspace = temp.path().join("workspace");
  fs::create_dir(&workspace).unwrap();
  let server = FakeServer::answer(vec![
    tool_response(
      "call_denied",
      "write",
      r#"{"path":"denied.txt","contents":"must not exist"}"#,
      None,
    ),
    text_response("denied as expected"),
  ]);
  let config = write_config(temp.path(), &server.base_url(), false);

  let output = run(&config, &workspace, "try to write");
  server.requests();
  assert!(
    output.status.success(),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  assert!(!workspace.join("denied.txt").exists());

  let layout = StateLayout::new(temp.path().join("state"));
  let session_id = layout.list_session_ids().unwrap().pop().unwrap();
  let trace = TraceJournal::read(&layout.trace_path(&session_id))
    .unwrap()
    .items;
  assert!(trace.iter().any(|entry| matches!(
    &entry.envelope.event,
    AgentEvent::ToolFailed(failed)
      if failed.call_id.as_str() == "call_denied" && failed.message.contains("approval is required")
  )));
  assert!(!trace.iter().any(|entry| matches!(
    &entry.envelope.event,
    AgentEvent::ToolStarted(started) if started.call_id.as_str() == "call_denied"
  )));
}

#[test]
fn one_shot_read_cannot_escape_the_workspace_or_leak_secret_bytes() {
  let temp = TempDir::new().unwrap();
  let workspace = temp.path().join("workspace");
  fs::create_dir(&workspace).unwrap();
  let secret = "outside-secret-value-91f7";
  let outside = temp.path().join("secret.txt");
  fs::write(&outside, secret).unwrap();
  let absolute_arguments = serde_json::json!({"path": outside.to_string_lossy()}).to_string();
  let server = FakeServer::answer(vec![
    tool_response("call_absolute", "read", &absolute_arguments, None),
    tool_response("call_parent", "read", r#"{"path":"../secret.txt"}"#, None),
    text_response("outside reads refused"),
  ]);
  let config = write_config(temp.path(), &server.base_url(), true);

  let output = run(&config, &workspace, "read outside");
  let requests = server.requests();
  assert!(
    output.status.success(),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  assert_eq!(requests.len(), 3);
  assert!(
    requests
      .iter()
      .all(|request| !request.body.contains(secret))
  );

  let layout = StateLayout::new(temp.path().join("state"));
  let session_id = layout.list_session_ids().unwrap().pop().unwrap();
  let trace = TraceJournal::read(&layout.trace_path(&session_id))
    .unwrap()
    .items;
  for call_id in ["call_absolute", "call_parent"] {
    assert!(trace.iter().any(|entry| matches!(
      &entry.envelope.event,
      AgentEvent::ToolFailed(failed)
        if failed.call_id.as_str() == call_id && failed.message.contains("outside the workspace")
    )));
  }
}

#[test]
fn invalid_workspace_is_rejected_before_any_provider_request() {
  let temp = TempDir::new().unwrap();
  let listener = TcpListener::bind("127.0.0.1:0").unwrap();
  listener.set_nonblocking(true).unwrap();
  let config = write_config(
    temp.path(),
    &format!("http://{}/v1", listener.local_addr().unwrap()),
    true,
  );

  let output = run(&config, &temp.path().join("missing"), "hello");
  assert!(!output.status.success());
  assert!(
    String::from_utf8_lossy(&output.stderr).contains("invalid workspace"),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  assert_eq!(
    listener.accept().unwrap_err().kind(),
    std::io::ErrorKind::WouldBlock
  );
}

#[cfg(target_os = "linux")]
#[test]
fn non_utf8_workspace_is_rejected_before_any_provider_request() {
  use std::{ffi::OsString, os::unix::ffi::OsStringExt};

  let temp = TempDir::new().unwrap();
  let workspace = temp.path().join(OsString::from_vec(vec![b'w', 0xff]));
  fs::create_dir(&workspace).unwrap();
  let listener = TcpListener::bind("127.0.0.1:0").unwrap();
  listener.set_nonblocking(true).unwrap();
  let config = write_config(
    temp.path(),
    &format!("http://{}/v1", listener.local_addr().unwrap()),
    true,
  );

  let output = run(&config, &workspace, "hello");
  assert!(!output.status.success());
  assert!(
    String::from_utf8_lossy(&output.stderr).contains("not valid UTF-8"),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  assert_eq!(
    listener.accept().unwrap_err().kind(),
    std::io::ErrorKind::WouldBlock
  );
}

#[test]
fn absent_primary_endpoint_and_invalid_json_exit_nonzero() {
  let temp = TempDir::new().unwrap();
  let workspace = temp.path().join("workspace");
  fs::create_dir(&workspace).unwrap();
  let no_endpoint = temp.path().join("no-endpoint.json");
  let config = RuntimeConfig::new(
    ModelRef::new("missing", "model"),
    temp.path().join("state").to_string_lossy(),
  );
  fs::write(&no_endpoint, serde_json::to_vec(&config).unwrap()).unwrap();
  let output = run(&no_endpoint, &workspace, "hello");
  assert!(!output.status.success());
  assert!(String::from_utf8_lossy(&output.stderr).contains("has no endpoint entry"));

  let invalid = temp.path().join("invalid.json");
  fs::write(&invalid, b"not json").unwrap();
  let output = run(&invalid, &workspace, "hello");
  assert!(!output.status.success());
  assert!(String::from_utf8_lossy(&output.stderr).contains("invalid config"));
}

#[test]
fn provider_and_durable_state_failures_exit_nonzero() {
  let temp = TempDir::new().unwrap();
  let workspace = temp.path().join("workspace");
  fs::create_dir(&workspace).unwrap();
  let server = FakeServer::answer(vec![
    status_response(503, "Unavailable", r#"{"error":{"message":"offline"}}"#),
    status_response(503, "Unavailable", r#"{"error":{"message":"offline"}}"#),
  ]);
  let config = write_config(temp.path(), &server.base_url(), true);
  let output = run(&config, &workspace, "hello");
  server.requests();
  assert!(!output.status.success());
  assert!(String::from_utf8_lossy(&output.stderr).contains("provider failure"));

  let blocked_root = temp.path().join("state-is-a-file");
  fs::write(&blocked_root, "not a directory").unwrap();
  let mut config_value: serde_json::Value =
    serde_json::from_slice(&fs::read(&config).unwrap()).unwrap();
  config_value["state_dir"] = blocked_root.to_string_lossy().into_owned().into();
  let blocked_config = temp.path().join("blocked-config.json");
  fs::write(&blocked_config, serde_json::to_vec(&config_value).unwrap()).unwrap();
  let output = run(&blocked_config, &workspace, "hello");
  assert!(!output.status.success());
  assert!(String::from_utf8_lossy(&output.stderr).contains("durable state"));
}

#[test]
fn help_argument_errors_and_bare_invocation_exit_without_configuration() {
  let binary = env!("CARGO_BIN_EXE_pi-rs");
  let bare = Command::new(binary).output().unwrap();
  assert!(bare.status.success());
  // A bare invocation names the commands rather than guessing one, so it has to list
  // all of them.
  let top_help = String::from_utf8_lossy(&bare.stdout);
  assert!(top_help.contains("Usage: pi-rs <command>"), "{top_help}");
  for command in [
    "run",
    "interactive",
    "trace",
    "skills",
    "prompts",
    "prompt",
  ] {
    assert!(
      top_help.contains(command),
      "{command} missing from: {top_help}"
    );
  }

  let help = Command::new(binary)
    .args(["run", "--help"])
    .output()
    .unwrap();
  assert!(help.status.success());
  assert!(String::from_utf8_lossy(&help.stdout).contains("one durable coding-agent turn"));

  let invalid = Command::new(binary)
    .args(["run", "--prompt", "hi"])
    .output()
    .unwrap();
  assert_eq!(invalid.status.code(), Some(2));
  assert!(String::from_utf8_lossy(&invalid.stderr).contains("--config is required"));
}

// --- surface flags -------------------------------------------------------------

/// Run with extra surface flags, under a pinned environment.
///
/// The surface resolves colour and width from the environment, so a test that inherited
/// the developer's `NO_COLOR` or `TERM` would render differently in CI than at a desk.
fn run_surface(config: &Path, cwd: &Path, prompt: &str, extra: &[&str]) -> Output {
  Command::new(env!("CARGO_BIN_EXE_pi-rs"))
    .args(["run", "--config"])
    .arg(config)
    .arg("--cwd")
    .arg(cwd)
    .args(["--prompt", prompt])
    .args(extra)
    .env_remove("NO_COLOR")
    .env_remove("TERM")
    .env_remove("CLICOLOR")
    .env_remove("CLICOLOR_FORCE")
    .output()
    .expect("run pi-rs")
}

/// One offline turn with its own workspace, state root, config and fake provider.
///
/// A run consumes its provider's responses and writes its own session, so two runs that
/// are meant to be compared need two scenarios.
struct Scenario {
  workspace: PathBuf,
  state: PathBuf,
  config: PathBuf,
  server: FakeServer,
  _temp: TempDir,
}

fn scenario(responses: Vec<String>) -> Scenario {
  scenario_exposing(responses, ReasoningExposure::Native)
}

/// A scenario whose endpoint declares a different reasoning exposure.
fn scenario_exposing(responses: Vec<String>, exposed_reasoning: ReasoningExposure) -> Scenario {
  let temp = TempDir::new().unwrap();
  let workspace = temp.path().join("workspace");
  fs::create_dir(&workspace).unwrap();
  let server = FakeServer::answer(responses);
  let config = write_config_exposing(temp.path(), &server.base_url(), true, exposed_reasoning);
  let state = temp.path().join("state");
  Scenario {
    workspace,
    state,
    config,
    server,
    _temp: temp,
  }
}

/// A turn that reasons, calls one mutating tool, then answers in a single long line.
fn reasoning_and_long_answer() -> Vec<String> {
  vec![
    tool_response(
      "call_write",
      "write",
      r#"{"path":"model.txt","contents":"from tool\n"}"#,
      Some("choose a file"),
    ),
    text_response(&"answer ".repeat(40)),
  ]
}

fn trace_events(state: &Path) -> Vec<AgentEvent> {
  let layout = StateLayout::new(state);
  let session_id = layout
    .list_session_ids()
    .unwrap()
    .pop()
    .expect("session id");
  TraceJournal::read(&layout.trace_path(&session_id))
    .unwrap()
    .items
    .into_iter()
    .map(|entry| entry.envelope.event)
    .collect()
}

#[test]
fn no_reasoning_flag_hides_reasoning_without_hiding_the_answer() {
  let scene = scenario(reasoning_and_long_answer());
  let output = run_surface(&scene.config, &scene.workspace, "go", &["--no-reasoning"]);
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(output.status.success(), "{stderr}");
  assert!(!stderr.contains("[reasoning"), "{stderr}");
  // Hiding reasoning is not hiding the work: the rest of the transcript is intact.
  assert!(stderr.contains("[tool] write"), "{stderr}");
  assert!(stderr.contains("[tool ok] write"), "{stderr}");
  assert!(String::from_utf8_lossy(&output.stdout).starts_with("answer answer"));
}

/// The provenance claim is a claim about where the text came from, and only the
/// endpoint's own declaration says. A hosted endpoint exposes a *summary* of
/// reasoning it keeps hidden, sends it in the same `reasoning_content` field a local
/// server uses for the model's actual thinking, and must not be rendered as if
/// hidden thought had been recovered.
#[test]
fn a_summary_only_endpoint_is_labelled_as_a_summary() {
  let scene = scenario_exposing(
    reasoning_and_long_answer(),
    ReasoningExposure::ProviderSummary,
  );
  let output = run_surface(&scene.config, &scene.workspace, "go", &[]);
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(output.status.success(), "{stderr}");
  assert!(
    stderr.contains("[provider summary] choose a file"),
    "{stderr}"
  );
  assert!(
    !stderr.contains("[reasoning] "),
    "a summary must not be shown as native thinking: {stderr}"
  );
  // The durable record carries the same claim, not the field it was decoded from.
  let provenance: Vec<&str> = trace_events(&scene.state)
    .iter()
    .filter_map(|event| match event {
      AgentEvent::ReasoningDelta(delta) => Some(delta.provenance.as_str()),
      _ => None,
    })
    .collect();
  assert!(
    !provenance.is_empty(),
    "the reasoning must still be recorded"
  );
  assert!(
    provenance.iter().all(|p| *p == "provider_summary"),
    "{provenance:?}"
  );
}

#[test]
fn silent_suppresses_the_surface_but_not_the_record() {
  let scene = scenario(reasoning_and_long_answer());
  let output = run_surface(&scene.config, &scene.workspace, "go", &["--silent"]);
  assert!(output.status.success(), "{:?}", output.stderr);
  assert!(
    output.stderr.is_empty(),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  // Suppressing the display must not suppress the answer...
  assert!(String::from_utf8_lossy(&output.stdout).starts_with("answer answer"));
  // ...nor the durable trace, which is the product.
  let events = trace_events(&scene.state);
  assert!(
    events
      .iter()
      .any(|event| matches!(event, AgentEvent::ReasoningDelta(_))),
    "reasoning must still be recorded: {events:?}"
  );
  assert!(
    events
      .iter()
      .any(|event| matches!(event, AgentEvent::ToolRequested(_))),
    "tool calls must still be recorded: {events:?}"
  );
}

#[test]
fn quiet_keeps_trouble_and_drops_routine_work() {
  let scene = scenario(reasoning_and_long_answer());
  let output = run_surface(&scene.config, &scene.workspace, "go", &["--quiet"]);
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(output.status.success(), "{stderr}");
  // Routine work is not news in a log someone scans for trouble.
  assert!(!stderr.contains("[tool ok]"), "{stderr}");
  assert!(!stderr.contains("[tool] write"), "{stderr}");
  assert!(!stderr.contains("[reasoning"), "{stderr}");
  assert!(!stderr.contains("> go"), "{stderr}");
  // The answer is not transcript, so quiet never touches it.
  assert!(String::from_utf8_lossy(&output.stdout).starts_with("answer answer"));
}

#[test]
fn quiet_still_reports_a_refused_mutating_tool() {
  // A policy refusal is news precisely in the log that asked for only news: the model
  // tried to change something and did not.
  let temp = TempDir::new().unwrap();
  let workspace = temp.path().join("workspace");
  fs::create_dir(&workspace).unwrap();
  let server = FakeServer::answer(vec![
    tool_response(
      "call_denied",
      "write",
      r#"{"path":"denied.txt","contents":"must not exist"}"#,
      None,
    ),
    text_response("denied as expected"),
  ]);
  // Mutating tools are not auto-approved, so the write is refused by policy.
  let config = write_config(temp.path(), &server.base_url(), false);

  let output = run_surface(&config, &workspace, "try to write", &["--quiet"]);
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(output.status.success(), "{stderr}");
  assert!(stderr.contains("[tool failed] write"), "{stderr}");
  assert!(stderr.contains("approval is required"), "{stderr}");
  assert!(!workspace.join("denied.txt").exists());
}

#[test]
fn verbose_prints_each_request_once_not_twice() {
  let scene = scenario(vec![
    tool_response(
      "call_write",
      "write",
      r#"{"path":"a.txt","contents":"x"}"#,
      None,
    ),
    tool_response("call_read", "read", r#"{"path":"a.txt"}"#, None),
    text_response("done"),
  ]);
  let output = run_surface(&scene.config, &scene.workspace, "go", &["--verbose"]);
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(output.status.success(), "{stderr}");
  let requests = scene.server.requests();
  assert_eq!(requests.len(), 3);
  // The live surface and the durable trace partition events. If that partition were
  // wrong, the same request line would print twice per request.
  assert_eq!(
    stderr.matches("[request] fake/agent").count(),
    3,
    "one line per request, no double printing: {stderr}"
  );
}

#[test]
fn width_wraps_stderr_but_never_the_answer() {
  let scene = scenario(reasoning_and_long_answer());
  let output = run_surface(&scene.config, &scene.workspace, "go", &["--width=40"]);
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(output.status.success(), "{stderr}");
  for line in stderr.lines() {
    assert!(line.chars().count() <= 40, "wider than requested: {line}");
  }
  // The answer is data for a pipe, not a paragraph for a terminal: it is never wrapped.
  let stdout = String::from_utf8_lossy(&output.stdout);
  assert_eq!(stdout.lines().count(), 1, "{stdout}");
  assert!(stdout.starts_with(&"answer ".repeat(40)));
}

#[test]
fn colour_is_a_projection_and_never_touches_the_answer() {
  // Piped output is monochrome by default: nobody wants escape codes in a log file.
  let scene = scenario(reasoning_and_long_answer());
  let plain = run_surface(&scene.config, &scene.workspace, "go", &[]);
  assert!(
    plain.status.success(),
    "{}",
    String::from_utf8_lossy(&plain.stderr)
  );
  assert!(
    !plain.stdout.contains(&0x1b),
    "{}",
    String::from_utf8_lossy(&plain.stdout)
  );
  assert!(
    !plain.stderr.contains(&0x1b),
    "{}",
    String::from_utf8_lossy(&plain.stderr)
  );

  // An explicit request wins over the environment, including NO_COLOR.
  let scene = scenario(reasoning_and_long_answer());
  let coloured = Command::new(env!("CARGO_BIN_EXE_pi-rs"))
    .args(["run", "--config"])
    .arg(&scene.config)
    .arg("--cwd")
    .arg(&scene.workspace)
    .args(["--prompt", "go"])
    .args(["--color=always"])
    .env("NO_COLOR", "1")
    .output()
    .expect("run pi-rs");
  let coloured_stderr = String::from_utf8_lossy(&coloured.stderr);
  assert!(coloured.stderr.contains(&0x1b), "{coloured_stderr}");
  // Even forced, colour stays on the diagnostic stream; the answer stays byte-faithful.
  assert!(
    !coloured.stdout.contains(&0x1b),
    "{}",
    String::from_utf8_lossy(&coloured.stdout)
  );
}

#[test]
fn the_answer_does_not_change_with_what_the_surface_shows() {
  let loud = scenario(reasoning_and_long_answer());
  let quiet = scenario(reasoning_and_long_answer());
  let loud = run_surface(&loud.config, &loud.workspace, "go", &["--verbose"]);
  let quiet = run_surface(&quiet.config, &quiet.workspace, "go", &["--silent"]);
  assert!(
    loud.status.success(),
    "{}",
    String::from_utf8_lossy(&loud.stderr)
  );
  assert_eq!(loud.stdout, quiet.stdout);
  assert!(!loud.stdout.is_empty());
}
