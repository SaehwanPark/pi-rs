//! A handle opened once, turned twice, closed once.
//!
//! The claim under test is the one an interactive loop depends on: turns after the
//! first must see the earlier turns. The evidence is the recorded request payload —
//! what the model was actually given — read back from a fake provider, plus the
//! durable session, which must be one session rather than two glued together.

use std::{
  fs,
  io::{Read, Write},
  net::{SocketAddr, TcpListener, TcpStream},
  path::{Path, PathBuf},
  thread,
  time::{Duration, Instant},
};

use pi_rs_core::{
  AgentEvent, ModelCapabilities, ModelEndpoint, ModelRef, ReasoningExposure, Role, RuntimeConfig,
};
use pi_rs_store::{StateLayout, TraceJournal, WritePolicy};
use tempfile::TempDir;

use super::*;
use crate::cli::RunArgs;

#[test]
fn a_second_turn_on_one_handle_sends_the_first_turn_with_it() {
  let temp = TempDir::new().unwrap();
  let workspace = temp.path().join("workspace");
  fs::create_dir(&workspace).unwrap();
  let server = FakeServer::answer(vec![answer("orchid noted"), answer("violet noted")]);
  let config = write_config(temp.path(), &server.base_url());
  // `RunArgs::prompt` belongs to a single-turn run; this caller supplies its own
  // prompts per turn, which is the whole point of the handle.
  let args = RunArgs {
    config,
    cwd: workspace.clone(),
    prompt: String::new(),
    surface: SurfaceArgs::default(),
  };

  open_session(&args.config, &args.cwd, &args.surface, |session| {
    session
      .turn("remember the word orchid")
      .map_err(|error| turn_error(&error))?;
    session
      .turn("what word did I ask you to remember")
      .map_err(|error| turn_error(&error))?;
    session.close().map_err(session_error)
  })
  .expect("two turns on one session");

  let requests = server.requests();
  assert_eq!(requests.len(), 2, "one model request per turn");
  // The first turn knows nothing it was not told.
  assert!(requests[0].contains("remember the word orchid"));
  assert!(
    !requests[0].contains("orchid noted"),
    "turn 1 cannot carry its own answer"
  );
  // The second turn carries the first turn's user message and its answer.
  assert!(requests[1].contains("remember the word orchid"));
  assert!(requests[1].contains("orchid noted"));
  assert!(requests[1].contains("what word did I ask you to remember"));

  // Memory is one session with a history, not two sessions in a row: one lifecycle
  // pair, and both turns recorded under it.
  let layout = StateLayout::new(temp.path().join("state"));
  let sessions = layout.list_session_ids().unwrap();
  assert_eq!(sessions.len(), 1, "one session for two turns");
  let trace = TraceJournal::read(&layout.trace_path(&sessions[0]))
    .unwrap()
    .items;
  let lifecycle = |starts: bool| {
    trace
      .iter()
      .filter(|entry| match &entry.envelope.event {
        AgentEvent::SessionStarted(_) => starts,
        AgentEvent::SessionEnded(_) => !starts,
        _ => false,
      })
      .count()
  };
  assert_eq!(
    lifecycle(true),
    1,
    "SessionStarted is not re-emitted per turn"
  );
  assert_eq!(lifecycle(false), 1, "closing once ends the session once");

  let restored = Store::new(layout.root(), WritePolicy::default())
    .restore(&sessions[0])
    .unwrap();
  let of = |role: Role| restored.messages.iter().filter(|m| m.role == role).count();
  assert_eq!(of(Role::User), 2, "both prompts recorded");
  assert_eq!(of(Role::Assistant), 2, "both answers recorded");
}

/// Answers a fixed list of completions and records every request body it served.
struct FakeServer {
  addr: SocketAddr,
  requests: thread::JoinHandle<Vec<String>>,
}

impl FakeServer {
  fn answer(responses: Vec<String>) -> Self {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake provider");
    let addr = listener.local_addr().expect("fake provider address");
    listener.set_nonblocking(true).unwrap();
    let requests = thread::spawn(move || {
      responses
        .into_iter()
        .map(|response| {
          let mut socket = accept(&listener);
          let request = drain_request(&mut socket);
          socket.write_all(response.as_bytes()).expect("write answer");
          socket.flush().expect("flush answer");
          // Dropping the socket here is what ends the response stream.
          request
        })
        .collect()
    });
    Self { addr, requests }
  }

  fn base_url(&self) -> String {
    format!("http://{}/v1", self.addr)
  }

  fn requests(self) -> Vec<String> {
    self.requests.join().expect("fake provider thread")
  }
}

fn accept(listener: &TcpListener) -> TcpStream {
  let deadline = Instant::now() + Duration::from_secs(10);
  loop {
    match listener.accept() {
      Ok((socket, _)) => return socket,
      Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
        assert!(Instant::now() < deadline, "timed out waiting for a request");
        thread::sleep(Duration::from_millis(5));
      }
      Err(error) => panic!("accept a provider request: {error}"),
    }
  }
}

/// Read one request, headers and body both, from a connection that stays open.
fn drain_request(socket: &mut TcpStream) -> String {
  let mut bytes = Vec::new();
  let mut buffer = [0u8; 1024];
  let mut expected: Option<usize> = None;
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
    if expected.is_some_and(|len| bytes.len() >= len) {
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

/// One streamed answer, in the shape the OpenAI-compatible adapter decodes.
fn answer(text: &str) -> String {
  let events = [
    serde_json::json!({ "choices": [{ "delta": { "content": text } }] }),
    serde_json::json!({
      "choices": [{ "delta": {}, "finish_reason": "stop" }],
      "usage": { "prompt_tokens": 20, "completion_tokens": 3 }
    }),
  ];
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

/// A config that addresses the fake provider, with its state inside `root`.
fn write_config(root: &Path, base_url: &str) -> PathBuf {
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
  let path = root.join("config.json");
  fs::write(&path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
  path
}
