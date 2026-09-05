use std::{
  fs,
  io::{Read, Write},
  net::{SocketAddr, TcpListener, TcpStream},
  path::{Path, PathBuf},
  process::{Command, Output},
  thread,
  time::{Duration, Instant},
};

use pi_rs_core::{
  AgentEvent, ModelCapabilities, ModelEndpoint, ModelRef, ReasoningExposure, Role, RuntimeConfig,
};
use pi_rs_store::{StateLayout, Store, TraceJournal, WritePolicy};
use tempfile::TempDir;

struct FakeServer {
  addr: SocketAddr,
  handle: thread::JoinHandle<Vec<String>>,
}

impl FakeServer {
  fn answer(responses: Vec<String>) -> Self {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake provider");
    let addr = listener.local_addr().expect("fake provider address");
    listener.set_nonblocking(true).unwrap();
    let handle = thread::spawn(move || {
      responses
        .into_iter()
        .map(|response| {
          let deadline = Instant::now() + Duration::from_secs(5);
          let mut socket = loop {
            match listener.accept() {
              Ok((socket, _)) => break socket,
              Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                  Instant::now() < deadline,
                  "timed out waiting for provider request"
                );
                thread::sleep(Duration::from_millis(5));
              }
              Err(error) => panic!("accept provider request: {error}"),
            }
          };
          let request = drain_request(&mut socket);
          socket
            .write_all(response.as_bytes())
            .expect("write response");
          socket.flush().expect("flush response");
          request
        })
        .collect()
    });
    Self { addr, handle }
  }

  fn base_url(&self) -> String {
    format!("http://{}/v1", self.addr)
  }

  fn requests(self) -> Vec<String> {
    self.handle.join().expect("fake provider thread")
  }
}

fn drain_request(socket: &mut TcpStream) -> String {
  let mut bytes = Vec::new();
  let mut buffer = [0u8; 1024];
  let mut expected = None;
  loop {
    let read = socket.read(&mut buffer).unwrap_or(0);
    if read == 0 {
      break;
    }
    bytes.extend_from_slice(&buffer[..read]);
    if expected.is_none()
      && let Some(end) = find(&bytes, b"\r\n\r\n")
    {
      let body_start = end + 4;
      let headers = String::from_utf8_lossy(&bytes[..body_start]).to_ascii_lowercase();
      let content_length = headers
        .split("\r\n")
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
      expected = Some(body_start + content_length);
    }
    if expected.is_some_and(|length| bytes.len() >= length) {
      break;
    }
  }
  String::from_utf8_lossy(&bytes).into_owned()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
  haystack
    .windows(needle.len())
    .position(|part| part == needle)
}

fn sse(events: &[serde_json::Value]) -> String {
  let mut body = String::new();
  for event in events {
    body.push_str("data: ");
    body.push_str(&event.to_string());
    body.push_str("\n\n");
  }
  body.push_str("data: [DONE]\n\n");
  format!(
    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
    body.len()
  )
}

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

fn text_response(text: &str) -> String {
  sse(&[
    serde_json::json!({"choices": [{"delta": {"content": text}}]}),
    serde_json::json!({
      "choices": [{"delta": {}, "finish_reason": "stop"}],
      "usage": {"prompt_tokens": 20, "completion_tokens": 3}
    }),
  ])
}

fn status_response(code: u16, reason: &str, body: &str) -> String {
  format!(
    "HTTP/1.1 {code} {reason}\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
    body.len()
  )
}

fn write_config(root: &Path, base_url: &str, auto_approve_mutating: bool) -> PathBuf {
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
      exposed_reasoning: ReasoningExposure::Native,
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
  assert_eq!(String::from_utf8_lossy(&output.stdout), "completed");
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(stderr.contains("[reasoning] choose a file"), "{stderr}");
  assert!(stderr.contains("[tool requested] write"), "{stderr}");
  assert!(
    stderr.contains("[tool finished] exec Succeeded"),
    "{stderr}"
  );
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
  assert!(requests.iter().all(|request| !request.contains(secret)));

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

#[cfg(unix)]
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
  assert!(String::from_utf8_lossy(&bare.stdout).contains("Usage: pi-rs run"));

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
