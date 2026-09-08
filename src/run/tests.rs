//! A handle opened once, turned twice, closed once.
//!
//! The claim under test is the one an interactive loop depends on: turns after the
//! first must see the earlier turns. The evidence is the recorded request payload —
//! what the model was actually given — read back from a fake provider, plus the
//! durable session, which must be one session rather than two glued together.
//!
//! Cancellation is tested here for the same reason: what a caller of `SessionHandle`
//! may not do is lose the session to a turn it interrupted, or learn the outcome of
//! a canceled turn by guessing from what was printed.

use std::{
  fs,
  io::{Read, Write},
  net::{SocketAddr, TcpListener, TcpStream},
  path::{Path, PathBuf},
  thread,
  time::{Duration, Instant},
};

use pi_rs_core::{
  AgentEvent, ContentBlock, ModelCapabilities, ModelEndpoint, ModelRef, ReasoningExposure, Role,
  RuntimeConfig, ToolExecutionState, TurnStatus,
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
    resume: None,
    surface: SurfaceArgs::default(),
  };

  open_session(&args.config, &args.cwd, &args.surface, None, |session| {
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

#[test]
fn a_canceled_turn_ends_cancelled_and_the_same_handle_answers_again() {
  let temp = TempDir::new().unwrap();
  let workspace = temp.path().join("workspace");
  fs::create_dir(&workspace).unwrap();
  // Five deltas a fifth of a second apart: the turn is still being answered long
  // after the cancel lands, and the frame that arrives just after it is what lets
  // the transport notice.
  let server = FakeServer::scripted(vec![
    dripping_answer(
      &["or", "chid", " is", " the", " word"],
      Duration::from_millis(200),
    ),
    Scripted::Whole(answer("marigold noted")),
  ]);
  let config = write_config(temp.path(), &server.base_url());
  let args = RunArgs {
    config,
    cwd: workspace.clone(),
    prompt: String::new(),
    resume: None,
    surface: SurfaceArgs::default(),
  };

  open_session(&args.config, &args.cwd, &args.surface, None, |session| {
    let cancel = CancelToken::new();
    interrupt_after(&cancel, Duration::from_millis(80));
    let started = Instant::now();
    let canceled = session
      .turn_with("name a flower", &cancel)
      .map_err(|error| turn_error(&error))?;
    // The stream would have run for a second. The bound is far below that and far
    // above a busy machine's scheduling jitter, so a turn that ignored the cancel
    // cannot pass it and a quiet machine does not fail it by accident.
    assert!(
      started.elapsed() < Duration::from_millis(500),
      "the canceled turn ran for {:?}",
      started.elapsed()
    );
    // Neither success nor a provider failure: the state the runtime defines for a
    // turn the user stopped.
    assert_eq!(canceled.status, TurnStatus::Cancelled);

    // A token is one-shot, so the next turn takes a fresh one — and gets an answer,
    // which is the session having survived its own interruption.
    let next = session
      .turn_with("what did you say", &CancelToken::new())
      .map_err(|error| turn_error(&error))?;
    assert_eq!(next.status, TurnStatus::Completed);
    assert_eq!(next.text, "marigold noted");
    session.close().map_err(session_error)
  })
  .expect("a canceled turn and the one after it");

  let requests = server.requests();
  assert_eq!(
    requests.len(),
    2,
    "the canceled turn, then the turn after it"
  );
  assert!(
    requests[1].contains("name a flower"),
    "the canceled turn stays in the history the next turn is built on"
  );
  assert_eq!(
    turn_statuses(temp.path()),
    vec![TurnStatus::Cancelled, TurnStatus::Completed],
    "the durable trace records the cancellation itself, in order"
  );
}

#[test]
fn a_cancel_during_a_mutating_tool_leaves_that_call_unknown() {
  let temp = TempDir::new().unwrap();
  let workspace = temp.path().join("workspace");
  fs::create_dir(&workspace).unwrap();
  // A command that runs long after the cancel lands, so the interrupt arrives while
  // the side effect is genuinely in progress.
  let server = FakeServer::scripted(vec![Scripted::Whole(tool_call(
    "exec",
    serde_json::json!({ "command": "sleep 0.7 && echo done" }),
  ))]);
  let config = write_config_with(temp.path(), &server.base_url(), |config| {
    // A mutating call is refused unless the policy answers for the user, and this
    // test is about a call that really ran.
    config.tools.auto_approve_mutating = true;
  });
  let args = RunArgs {
    config,
    cwd: workspace.clone(),
    prompt: String::new(),
    resume: None,
    surface: SurfaceArgs::default(),
  };

  open_session(&args.config, &args.cwd, &args.surface, None, |session| {
    let cancel = CancelToken::new();
    interrupt_after(&cancel, Duration::from_millis(150));
    let canceled = session
      .turn_with("change something", &cancel)
      .map_err(|error| turn_error(&error))?;
    assert_eq!(canceled.status, TurnStatus::Cancelled);
    assert_eq!(canceled.tool_calls, 1, "the call was made, not skipped");
    session.close().map_err(session_error)
  })
  .expect("a turn canceled inside a tool");

  // What the session will hand to the next model, and to a resume.
  let layout = StateLayout::new(temp.path().join("state"));
  let sessions = layout.list_session_ids().unwrap();
  let restored = Store::new(layout.root(), WritePolicy::default())
    .restore(&sessions[0])
    .unwrap();
  let results: Vec<(String, ToolExecutionState, String)> = restored
    .messages
    .iter()
    .flat_map(|message| message.message.content.iter())
    .filter_map(|block| match block {
      ContentBlock::ToolResult(result) => {
        Some((result.name.clone(), result.state, result.text.clone()))
      }
      _ => None,
    })
    .collect();
  assert_eq!(results.len(), 1, "the one call the model asked for");
  let (name, state, text) = &results[0];
  assert_eq!(name, "exec");
  // The command ran out its whole and `done` is its output, and the state is still
  // not `Succeeded`: a mutating call interrupted while it was running may be
  // half-applied, and the runtime does not upgrade its own ignorance.
  assert!(text.contains("done"), "the command really ran: {text}");
  assert_eq!(
    *state,
    ToolExecutionState::Unknown,
    "cancellation is never rewritten as success: {text}"
  );

  let events = recorded_events(temp.path());
  assert!(
    !events
      .iter()
      .any(|event| matches!(event, AgentEvent::ToolCompleted(_))),
    "no completed-tool event for a call whose end was not observed"
  );
  assert!(
    events
      .iter()
      .any(|event| matches!(event, AgentEvent::ToolUnknown(_))),
    "the uncertainty is a semantic event in the trace, not a log line"
  );
}

/// How one scripted reply reaches the client.
enum Scripted {
  /// Written in one piece. Dropping the socket afterwards is what ends the stream.
  Whole(String),
  /// Written one SSE frame at a time, `gap` apart, the first frame carrying the
  /// status line and headers.
  ///
  /// A cancel is only observable when a read returns, so a turn that is to be
  /// interrupted mid-answer needs a stream that keeps arriving. A body written in one
  /// piece is decoded before a caller could react, and the turn then ends because the
  /// answer finished — which is not the thing under test.
  Drip { frames: Vec<String>, gap: Duration },
}

/// Answers a fixed list of completions and records every request body it served.
struct FakeServer {
  addr: SocketAddr,
  requests: thread::JoinHandle<Vec<String>>,
}

impl FakeServer {
  fn answer(responses: Vec<String>) -> Self {
    Self::scripted(responses.into_iter().map(Scripted::Whole).collect())
  }

  fn scripted(responses: Vec<Scripted>) -> Self {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake provider");
    let addr = listener.local_addr().expect("fake provider address");
    listener.set_nonblocking(true).unwrap();
    let requests = thread::spawn(move || {
      responses
        .into_iter()
        .map(|response| {
          let mut socket = accept(&listener);
          let request = drain_request(&mut socket);
          match response {
            Scripted::Whole(body) => {
              socket.write_all(body.as_bytes()).expect("write answer");
            }
            Scripted::Drip { frames, gap } => {
              for frame in frames {
                // A canceled turn hangs up mid-stream, and the write that finds
                // nobody reading is the expected end of that test rather than a
                // failure of it. The recorded request is still what is returned.
                let _ = socket.write_all(frame.as_bytes());
                let _ = socket.flush();
                thread::sleep(gap);
              }
            }
          }
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
  framed(&body_of(&[delta(text), end_of_stream()]))
}

/// One answer delivered frame by frame: each piece of `texts` in its own frame,
/// `gap` apart, then the closing frame and the sentinel.
fn dripping_answer(texts: &[&str], gap: Duration) -> Scripted {
  let mut frames: Vec<String> = texts.iter().map(|text| frame(&delta(text))).collect();
  frames.push(frame(&end_of_stream()));
  frames.push(DONE_FRAME.to_string());
  // The status line and headers ride in front of the first frame; the declared
  // length covers every frame, including the ones still to come.
  let head = format!(
    "{}{}",
    headers(frames.iter().map(String::len).sum()),
    frames[0]
  );
  frames[0] = head;
  Scripted::Drip { frames, gap }
}

/// One `exec` call, asked for instead of an answer.
fn tool_call(name: &str, arguments: serde_json::Value) -> String {
  framed(&body_of(&[
    serde_json::json!({
      "choices": [{
        "delta": {
          "tool_calls": [{
            "index": 0,
            "id": "call-1",
            "function": { "name": name, "arguments": arguments.to_string() }
          }]
        }
      }]
    }),
    serde_json::json!({
      "choices": [{ "delta": {}, "finish_reason": "tool_calls" }],
      "usage": { "prompt_tokens": 20, "completion_tokens": 5 }
    }),
  ]))
}

fn delta(text: &str) -> serde_json::Value {
  serde_json::json!({ "choices": [{ "delta": { "content": text } }] })
}

fn end_of_stream() -> serde_json::Value {
  serde_json::json!({
    "choices": [{ "delta": {}, "finish_reason": "stop" }],
    "usage": { "prompt_tokens": 20, "completion_tokens": 3 }
  })
}

fn frame(event: &serde_json::Value) -> String {
  format!("data: {}\n\n", event)
}

/// The frame that ends a stream, in the shape the sentinel takes.
const DONE_FRAME: &str = "data: [DONE]\n\n";

fn body_of(events: &[serde_json::Value]) -> String {
  let mut body = String::new();
  for event in events {
    body.push_str("data: ");
    body.push_str(&event.to_string());
    body.push_str("\n\n");
  }
  body.push_str("data: [DONE]\n\n");
  body
}

fn framed(body: &str) -> String {
  format!("{}{body}", headers(body.len()))
}

fn headers(content_length: usize) -> String {
  format!(
    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\ncontent-length: {content_length}\r\n\r\n"
  )
}

/// Set `cancel` on its own thread, `delay` from now.
///
/// The delay is how a test gets a cancel in the middle of a turn: the turn must be
/// running on the calling thread, because that is the only thread a handle may be
/// used from.
fn interrupt_after(cancel: &CancelToken, delay: Duration) {
  let cancel = cancel.clone();
  thread::spawn(move || {
    thread::sleep(delay);
    cancel.cancel();
  });
}

/// The `turn_completed` status of each turn, in the order the durable trace wrote
/// them. A canceled turn has to be findable here, because this record — not what a
/// surface happened to print — is what a resume and a reader both trust.
fn turn_statuses(root: &Path) -> Vec<TurnStatus> {
  let layout = StateLayout::new(root.join("state"));
  let sessions = layout.list_session_ids().unwrap();
  assert_eq!(sessions.len(), 1, "one session for the whole test");
  TraceJournal::read(&layout.trace_path(&sessions[0]))
    .unwrap()
    .items
    .iter()
    .filter_map(|entry| match &entry.envelope.event {
      AgentEvent::TurnCompleted(event) => Some(event.status.clone()),
      _ => None,
    })
    .collect()
}

/// The events the single durable session recorded, in order.
fn recorded_events(root: &Path) -> Vec<AgentEvent> {
  let layout = StateLayout::new(root.join("state"));
  let sessions = layout.list_session_ids().unwrap();
  assert_eq!(sessions.len(), 1, "one session for the whole test");
  TraceJournal::read(&layout.trace_path(&sessions[0]))
    .unwrap()
    .items
    .iter()
    .map(|entry| entry.envelope.event.clone())
    .collect()
}

/// A config that addresses the fake provider, with its state inside `root`.
fn write_config(root: &Path, base_url: &str) -> PathBuf {
  write_config_with(root, base_url, |_| {})
}

/// As [`write_config`], with one change applied before the file is written.
fn write_config_with(root: &Path, base_url: &str, change: impl Fn(&mut RuntimeConfig)) -> PathBuf {
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
  change(&mut config);
  let path = root.join("config.json");
  fs::write(&path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
  path
}
