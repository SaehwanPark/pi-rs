//! Failover through the real binary: a primary that cannot serve, a backup that
//! answers, and a durable record of which model produced what.
//!
//! The fake-provider harness is duplicated from `tests/run_cli.rs` rather than
//! shared: integration tests are separate binaries, and pulling one green,
//! CI-verified file into a shared module is a refactor this slice does not need.

use std::{
  fs,
  io::{Read, Write},
  net::{SocketAddr, TcpListener, TcpStream},
  path::{Path, PathBuf},
  process::{Command, Output},
  thread,
  time::{Duration, Instant},
};

use pi_rs_core::{ModelCapabilities, ModelEndpoint, ModelRef, ReasoningExposure, RuntimeConfig};
use pi_rs_store::{StateLayout, TraceJournal};
use tempfile::TempDir;

/// One request the fake server saw. The body is the prompt the model actually
/// received, which is how a test proves what crossed a failover boundary.
#[derive(Clone)]
struct Observed {
  line: String,
  body: String,
}

struct FakeServer {
  addr: SocketAddr,
  handle: thread::JoinHandle<Vec<Observed>>,
}

impl FakeServer {
  /// Serves `responses`, one per request, and records the requests.
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

  fn requests(self) -> Vec<Observed> {
    self.handle.join().expect("fake provider thread")
  }
}

fn drain_request(socket: &mut TcpStream) -> Observed {
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
  let text = String::from_utf8_lossy(&bytes).into_owned();
  let line = text.lines().next().unwrap_or("POST (unread)").to_string();
  let body = match text.rfind("\r\n\r\n") {
    Some(end) => text[end + 4..].to_string(),
    None => String::new(),
  };
  Observed { line, body }
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

fn text_response(text: &str) -> String {
  sse(&[
    serde_json::json!({"choices": [{"delta": {"content": text}}]}),
    serde_json::json!({
      "choices": [{"delta": {}, "finish_reason": "stop"}],
      "usage": {"prompt_tokens": 20, "completion_tokens": 3}
    }),
  ])
}

/// A response that asks for a tool, then ends the stream. The adapter turns the
/// fragments into one call when the stream ends.
fn tool_call(id: &str, name: &str, arguments: &str) -> String {
  sse(&[serde_json::json!({
    "choices": [{
      "delta": {"tool_calls": [{
        "id": id,
        "type": "function",
        "function": {"name": name, "arguments": arguments}
      }]},
      "finish_reason": "tool_calls"
    }],
    "usage": {"prompt_tokens": 20, "completion_tokens": 4}
  })])
}

fn unavailable() -> String {
  status_response(503, "Service Unavailable", r#"{"error":"down"}"#)
}

fn status_response(code: u16, reason: &str, body: &str) -> String {
  format!(
    "HTTP/1.1 {code} {reason}\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
    body.len()
  )
}

/// Primary `fake/agent`, backup `fake/standby` (or a backup that cannot be built).
fn write_config(root: &Path, primary_url: &str, backup: Backup) -> PathBuf {
  write_config_from(root, &config_at(root, primary_url, backup))
}

fn write_config_from(root: &Path, config: &RuntimeConfig) -> PathBuf {
  let path = root.join("config.json");
  fs::write(&path, serde_json::to_vec_pretty(config).unwrap()).unwrap();
  path
}

fn config_at(root: &Path, primary_url: &str, backup: Backup) -> RuntimeConfig {
  let state = root.join("state");
  let mut config = RuntimeConfig::new(ModelRef::new("fake", "agent"), state.to_string_lossy());
  config
    .endpoints
    .push(endpoint("agent", Some(primary_url.into())));
  match backup {
    Backup::Served(url) => {
      config.backup = Some(ModelRef::new("fake", "standby"));
      config.endpoints.push(endpoint("standby", Some(url)));
    }
    Backup::Unbuildable => {
      // Addresses no HTTP endpoint: `RuntimeConfig::validate` accepts it, and only
      // constructing the adapter finds out.
      config.backup = Some(ModelRef::new("fake", "broken"));
      config.endpoints.push(endpoint("broken", None));
    }
    Backup::None => {}
  }
  config
}

enum Backup {
  Served(String),
  Unbuildable,
  None,
}

fn endpoint(model: &str, base_url: Option<String>) -> ModelEndpoint {
  ModelEndpoint {
    provider: "fake".into(),
    model: model.into(),
    base_url,
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
  }
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

/// One recorded event with the attribution the stage gate asks about.
///
/// `epoch`/`model` are envelope attribution: which model was in charge when the
/// event was written. `about_epoch`/`about_model` are what the event *describes*.
/// They differ for a transition event, which the leaving epoch records.
struct Recorded {
  kind: String,
  epoch: Option<u32>,
  model: Option<String>,
  about_epoch: Option<u64>,
  about_model: Option<String>,
}

fn trace(state: &Path) -> Vec<Recorded> {
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
    .map(|entry| {
      let payload = serde_json::to_value(&entry.envelope.event).unwrap();
      Recorded {
        kind: payload
          .get("type")
          .and_then(|value| value.as_str())
          .unwrap_or("?")
          .to_string(),
        epoch: entry.envelope.meta.model_epoch,
        model: entry.envelope.meta.model.as_ref().map(ModelRef::as_key),
        about_epoch: payload.get("epoch").and_then(|value| value.as_u64()),
        about_model: payload
          .get("model")
          .and_then(|value| value.as_str())
          .map(str::to_string),
      }
    })
    .collect()
}

/// A session whose primary is down: two refused requests, then whatever the backup
/// endpoint can do.
struct Takeover {
  _temp: TempDir,
  config: PathBuf,
  workspace: PathBuf,
  state: PathBuf,
  primary: FakeServer,
  standby: FakeServer,
}

/// Primary answers 503 twice; the standby serves `answers`.
fn takeover(answers: Vec<String>) -> Takeover {
  let primary = FakeServer::answer(vec![unavailable(), unavailable()]);
  let standby = FakeServer::answer(answers);
  let url = standby.base_url();
  takeover_at(
    TempDir::new().expect("state root"),
    primary,
    standby,
    Backup::Served(url),
  )
}

/// Primary answers 503 twice; the backup endpoint cannot build an adapter.
fn takeover_into_unbuildable() -> Takeover {
  let primary = FakeServer::answer(vec![unavailable(), unavailable()]);
  let standby = FakeServer::answer(Vec::new());
  takeover_at(
    TempDir::new().expect("state root"),
    primary,
    standby,
    Backup::Unbuildable,
  )
}

fn takeover_at(
  temp: TempDir,
  primary: FakeServer,
  standby: FakeServer,
  backup: Backup,
) -> Takeover {
  let root = temp.path().to_path_buf();
  let workspace = root.join("workspace");
  fs::create_dir_all(&workspace).expect("workspace");
  let config = write_config(&root, &primary.base_url(), backup);
  Takeover {
    _temp: temp,
    config,
    workspace,
    state: root.join("state"),
    primary,
    standby,
  }
}

#[test]
fn a_down_primary_hands_the_turn_to_the_backup() {
  let scene = takeover(vec![text_response("served by the standby")]);
  let output = run(&scene.config, &scene.workspace, "go");
  let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
  let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
  assert!(output.status.success(), "{stderr}");
  assert_eq!(stdout, "served by the standby\n");
  // Takeover is a rare event, so the default surface reports it without `--verbose`.
  assert!(stderr.contains("[failover]"), "{stderr}");
  assert!(stderr.contains("fake/agent"), "{stderr}");
  assert!(stderr.contains("fake/standby"), "{stderr}");
  assert!(stderr.contains("provider_unavailable"), "{stderr}");
  // The primary got its full attempt budget, and only then did the work move.
  assert_eq!(scene.primary.requests().len(), 2, "retry, then yield");
  assert_eq!(scene.standby.requests().len(), 1);
}

#[test]
fn a_failover_records_the_epoch_that_served_the_answer() {
  let scene = takeover(vec![text_response("served by the standby")]);
  let output = run(&scene.config, &scene.workspace, "go");
  let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
  assert!(output.status.success(), "{stderr}");

  let events = trace(&scene.state);
  let requests: Vec<&Recorded> = events
    .iter()
    .filter(|event| event.kind == "model_request_started")
    .collect();
  assert_eq!(
    requests.len(),
    3,
    "two primary attempts, one takeover: {stderr}"
  );
  for attempt in &requests[..2] {
    assert_eq!(attempt.epoch, Some(0), "primary attempts stay in epoch 0");
    assert_eq!(attempt.model.as_deref(), Some("fake/agent"));
  }
  let served = requests[2];
  assert_eq!(served.epoch, Some(1), "the backup opens epoch 1");
  assert_eq!(served.model.as_deref(), Some("fake/standby"));

  let turn = events
    .iter()
    .find(|event| event.kind == "turn_completed")
    .expect("turn completed");
  assert_eq!(
    turn.epoch,
    Some(1),
    "the turn belongs to the model that finished it"
  );
  assert_eq!(turn.model.as_deref(), Some("fake/standby"));

  let epochs: Vec<&Recorded> = events
    .iter()
    .filter(|event| event.kind == "model_epoch_started")
    .collect();
  assert_eq!(epochs.len(), 2, "initial epoch plus the takeover");
  assert_eq!(epochs[0].about_epoch, Some(0));
  assert_eq!(epochs[0].about_model.as_deref(), Some("fake/agent"));
  assert_eq!(epochs[1].about_epoch, Some(1));
  assert_eq!(epochs[1].about_model.as_deref(), Some("fake/standby"));
  // The transition is recorded by the epoch it leaves, so its own attribution is
  // epoch 0 while it describes epoch 1. Reading the two as one field would either
  // lose who caused the switch or pretend the new model announced itself.
  assert_eq!(epochs[1].epoch, Some(0));
}

#[test]
fn an_untouched_backup_is_never_built() {
  // This backup endpoint cannot be built at all. If a configured backup were
  // initialized at startup, the session would fail before it began; deferral means a
  // healthy primary is entirely unaffected by a backup it never uses.
  let temp = TempDir::new().expect("state root");
  let root = temp.path();
  let primary = FakeServer::answer(vec![text_response("primary was enough")]);
  let workspace = root.join("workspace");
  fs::create_dir_all(&workspace).expect("workspace");
  let config = write_config(root, &primary.base_url(), Backup::Unbuildable);
  let output = run(&config, &workspace, "go");
  let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
  let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
  assert!(output.status.success(), "{stderr}");
  assert_eq!(stdout, "primary was enough\n");
  assert!(!stderr.contains("initialized"), "{stderr}");
  assert!(!stderr.contains("fake/broken"), "{stderr}");
  assert_eq!(primary.requests().len(), 1);
}

#[test]
fn a_backup_that_cannot_be_built_fails_honestly() {
  let scene = takeover_into_unbuildable();
  let output = run(&scene.config, &scene.workspace, "go");
  let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
  assert!(
    !output.status.success(),
    "nothing served this turn: {stderr}"
  );
  // The takeover is still recorded: the runtime did decide to move, and the record
  // must not pretend the primary was still in charge.
  assert!(stderr.contains("[failover]"), "{stderr}");
  assert!(stderr.contains("could not be initialized"), "{stderr}");
  assert!(stderr.contains("base_url"), "{stderr}");
  assert!(
    stderr.contains("refused: it is the active model"),
    "a second takeover into the same model must be named: {stderr}"
  );
  assert_eq!(scene.primary.requests().len(), 2);
  assert!(
    scene.standby.requests().is_empty(),
    "an unbuildable backup must not be addressed"
  );
}

#[test]
fn no_backup_means_a_down_primary_is_simply_fatal() {
  let temp = TempDir::new().expect("state root");
  let root = temp.path();
  let primary = FakeServer::answer(vec![unavailable(), unavailable()]);
  let workspace = root.join("workspace");
  fs::create_dir_all(&workspace).expect("workspace");
  let config = write_config(root, &primary.base_url(), Backup::None);
  let output = run(&config, &workspace, "go");
  let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
  assert!(!output.status.success(), "{stderr}");
  assert!(!stderr.contains("[failover]"), "{stderr}");
  assert!(stderr.contains("provider_unavailable"), "{stderr}");
  // Two attempts against the only model available, and no silent third.
  assert_eq!(primary.requests().len(), 2);
  let events = trace(&root.join("state"));
  assert!(
    !events
      .iter()
      .any(|event| event.kind == "model_failover" || event.kind == "model_epoch_started" && false),
    "no backup, no epoch transition"
  );
  assert_eq!(
    events
      .iter()
      .filter(|event| event.kind == "model_epoch_started")
      .count(),
    1,
    "only the initial epoch"
  );
}

#[test]
fn a_committed_tool_result_crosses_the_failover_boundary() {
  // The hardest case in the failover phase: the model asks for a side effect, the
  // side effect commits, and only then does the model go away. The backup must
  // continue from the recorded result, and the primary must not be asked to redo
  // the call.
  //
  // The operator, not the model, grants mutation. Without this the registry refuses
  // the write, which is the correct answer to a different question.
  let primary = FakeServer::answer(vec![
    tool_call(
      "call_write",
      "write",
      r#"{"path":"note.txt","contents":"from the primary"}"#,
    ),
    unavailable(),
  ]);
  let standby = FakeServer::answer(vec![text_response("the note is saved")]);
  let standby_url = standby.base_url();
  let temp = TempDir::new().expect("workspace");
  let root = temp.path().to_path_buf();
  let workspace = root.join("workspace");
  fs::create_dir_all(&workspace).expect("workspace");
  let mut config = config_at(&root, &primary.base_url(), Backup::Served(standby_url));
  config.tools.auto_approve_mutating = true;
  let config = write_config_from(&root, &config);

  let output = run(&config, &workspace, "save a note");
  let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
  let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
  assert!(output.status.success(), "{stderr}");
  assert_eq!(stdout, "the note is saved\n");

  // The write happened, once.
  assert_eq!(
    fs::read(workspace.join("note.txt")).expect("the written file"),
    b"from the primary"
  );
  assert_eq!(
    primary.requests().len(),
    2,
    "the tool call was issued once and never re-issued to the primary"
  );

  // The backup continued from the committed result rather than from a re-ask.
  let asked = standby.requests().pop().expect("the backup was asked");
  assert!(
    asked.line.contains("POST /v1/chat/completions"),
    "the backup is addressed through its endpoint's own path: {}",
    asked.line
  );
  let served = asked.body;
  assert!(
    served.contains(r#""role":"tool""#),
    "the committed tool result must travel to the backup: {served}"
  );
  assert!(
    served.contains("note.txt"),
    "the result names the file: {served}"
  );

  // And each major event keeps the model that produced it.
  let events = trace(&root.join("state"));
  let completions: Vec<&Recorded> = events
    .iter()
    .filter(|event| event.kind == "tool_completed")
    .collect();
  assert_eq!(completions.len(), 1, "one completion for one call");
  assert_eq!(
    completions[0].model.as_deref(),
    Some("fake/agent"),
    "the write belongs to the model that asked for it"
  );
  assert_eq!(completions[0].epoch, Some(0));
  let turn = events
    .iter()
    .find(|event| event.kind == "turn_completed")
    .expect("turn completed");
  assert_eq!(turn.model.as_deref(), Some("fake/standby"));
}
