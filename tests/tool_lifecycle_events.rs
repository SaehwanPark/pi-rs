//! Prove that a tool action leaves durable lifecycle records in the trace journal.
//!
//! Two invariants, checked end-to-end: the real binary drives a turn that performs
//! more than one tool action against the deterministic fake provider, and the trace
//! is then read back out of the store. What the journal must show is that
//!
//! 1. one durable call id ties every record of one tool action together, and
//! 2. each tool action leaves exactly one terminal record, ordered after the record
//!    that started it.

use std::{
  fs,
  io::{Read, Write},
  net::{SocketAddr, TcpListener, TcpStream},
  path::{Path, PathBuf},
  process::Command,
  thread,
  time::{Duration, Instant},
};

use pi_rs_core::{
  AgentEvent, ModelCapabilities, ModelEndpoint, ModelRef, ReasoningExposure, RuntimeConfig,
};
use pi_rs_store::{StateLayout, TraceJournal};
use tempfile::TempDir;

/// Provider-supplied call ids. That these exact ids reach the journal is part of the
/// durable-id claim: the id the model emitted is the id stored on disk.
const WRITE_CALL: &str = "call_write";
const EXEC_CALL: &str = "call_exec";

/// Every record of the `write` action carries one id, and that id is the one the
/// provider emitted.
#[test]
fn one_durable_call_id_ties_every_record_of_a_tool_action() {
  let records = tool_turn_records();

  let mut action_ids: Vec<&str> = Vec::new();
  for (name, provider_id) in [("write", WRITE_CALL), ("exec", EXEC_CALL)] {
    let action: Vec<&Record> = records
      .iter()
      .filter(|record| tool_name(&record.event) == Some(name))
      .collect();
    assert!(
      action.len() > 1,
      "{name} left {action:?}, which is not a lifecycle a shared id could tie together",
    );

    let ids: Vec<&str> = action
      .iter()
      .map(|record| call_id(&record.event).expect("lifecycle record carries a call id"))
      .collect();
    assert!(
      ids.iter().all(|id| *id == ids[0]),
      "{name} records were tied to more than one id: {ids:?}"
    );
    assert_eq!(
      ids[0], provider_id,
      "{name} persisted a different id than the provider supplied"
    );

    if !action_ids.contains(&ids[0]) {
      action_ids.push(ids[0]);
    }
  }

  // Two tool actions, two ids: an id must not tie two actions together.
  assert_eq!(action_ids.len(), 2, "actions shared an id: {action_ids:?}");
}

/// Per tool action: exactly one terminal record, and it is sequenced after the
/// record that started the action.
#[test]
fn each_tool_action_leaves_one_terminal_record_after_its_start() {
  let records = tool_turn_records();

  // The journal's own sequence is what orders a later reader.
  assert!(
    records.windows(2).all(|pair| pair[0].seq < pair[1].seq),
    "lifecycle sequences are not strictly increasing: {:?}",
    records.iter().map(|record| record.seq).collect::<Vec<_>>()
  );

  let mut ids: Vec<&str> = Vec::new();
  for record in &records {
    let id = call_id(&record.event).expect("lifecycle record carries a call id");
    if !ids.contains(&id) {
      ids.push(id);
    }
  }
  assert!(
    ids.len() > 1,
    "the turn performed one tool action, not more: {ids:?}"
  );

  for id in ids {
    let action: Vec<&Record> = records
      .iter()
      .filter(|record| call_id(&record.event) == Some(id))
      .collect();
    let starts: Vec<&&Record> = action
      .iter()
      .filter(|record| matches!(stage(&record.event), Some(Stage::Start)))
      .collect();
    let terminals: Vec<&&Record> = action
      .iter()
      .filter(|record| matches!(stage(&record.event), Some(Stage::Terminal)))
      .collect();

    assert_eq!(
      terminals.len(),
      1,
      "tool action {id} left {} terminal records: {terminals:?}",
      terminals.len(),
    );
    assert!(
      !starts.is_empty(),
      "tool action {id} recorded a terminal state without a start: {action:?}"
    );
    assert!(
      starts[0].seq < terminals[0].seq,
      "tool action {id} started at sequence {} but closed at sequence {}",
      starts[0].seq,
      terminals[0].seq,
    );
  }
}

/// Drive one real turn that performs two tool actions, then read the lifecycle
/// records back out of the store the way a later process would.
fn tool_turn_records() -> Vec<Record> {
  let temp = TempDir::new().expect("temp root");
  let workspace = temp.path().join("workspace");
  fs::create_dir(&workspace).expect("create workspace");
  let server = FakeServer::answer(vec![
    tool_response(
      WRITE_CALL,
      "write",
      r#"{"path":"model.txt","contents":"from tool\n"}"#,
    ),
    tool_response(
      EXEC_CALL,
      "exec",
      r#"{"command":"printf executed > exec.txt"}"#,
    ),
    text_response("completed"),
  ]);
  // Both tools are mutating, so the run needs explicit auto-approval to reach them.
  let config = write_config(temp.path(), &server.base_url(), true);
  let state = temp.path().join("state");

  let output = run(&config, &workspace, "make the files");
  let requests = server.requests();
  assert!(
    output.status.success(),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  // Two tool calls plus the closing answer is the shape of a multi-action turn.
  assert_eq!(requests.len(), 3, "the turn was not a multi-action turn");

  let records = lifecycle_records(&state);
  drop(temp);
  records
}

/// A lifecycle record and the sequence the store assigned it.
fn lifecycle_records(state: &Path) -> Vec<Record> {
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
    .map(|entry| Record {
      seq: entry.envelope.meta.seq.expect("store sequence").0,
      event: entry.envelope.event,
    })
    .filter(|record| call_id(&record.event).is_some())
    .collect()
}

/// The durable tool-call id a lifecycle record carries, if it is one.
fn call_id(event: &AgentEvent) -> Option<&str> {
  let id = match event {
    AgentEvent::ToolRequested(event) => &event.call_id,
    AgentEvent::ToolStarted(event) => &event.call_id,
    AgentEvent::ToolCompleted(event) => &event.call_id,
    AgentEvent::ToolFailed(event) => &event.call_id,
    AgentEvent::ToolUnknown(event) => &event.call_id,
    _ => return None,
  };
  Some(id.as_str())
}

/// The tool an action record names, for grouping records by tool action.
fn tool_name(event: &AgentEvent) -> Option<&str> {
  match event {
    AgentEvent::ToolRequested(event) => Some(&event.name),
    AgentEvent::ToolStarted(event) => Some(&event.name),
    AgentEvent::ToolCompleted(event) => Some(&event.name),
    AgentEvent::ToolFailed(event) => Some(&event.name),
    AgentEvent::ToolUnknown(event) => Some(&event.name),
    _ => None,
  }
}

/// Whether a record opens or closes a tool action.
fn stage(event: &AgentEvent) -> Option<Stage> {
  match event {
    AgentEvent::ToolRequested(_) | AgentEvent::ToolStarted(_) => Some(Stage::Start),
    AgentEvent::ToolCompleted(_) | AgentEvent::ToolFailed(_) | AgentEvent::ToolUnknown(_) => {
      Some(Stage::Terminal)
    }
    _ => None,
  }
}

/// One lifecycle record as the journal stored it.
#[derive(Debug)]
struct Record {
  /// Store-assigned sequence: the durable order, not the reader's iteration order.
  seq: u64,
  event: AgentEvent,
}

/// Where a record sits in one tool action's lifecycle.
enum Stage {
  /// The action is open.
  Start,
  /// The action is closed.
  Terminal,
}

/// Scripted OpenAI-compatible provider: one canned response per model request.
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

fn tool_response(id: &str, name: &str, arguments: &str) -> String {
  sse(&[
    serde_json::json!({
      "choices": [{"delta": {"tool_calls": [{
        "index": 0,
        "id": id,
        "function": {"name": name, "arguments": arguments}
      }]}}]
    }),
    serde_json::json!({
      "choices": [{"delta": {}, "finish_reason": "tool_calls"}]
    }),
  ])
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

fn run(config: &Path, cwd: &Path, prompt: &str) -> std::process::Output {
  Command::new(env!("CARGO_BIN_EXE_pi-rs"))
    .args(["run", "--config"])
    .arg(config)
    .arg("--cwd")
    .arg(cwd)
    .args(["--prompt", prompt])
    .output()
    .expect("run pi-rs")
}
