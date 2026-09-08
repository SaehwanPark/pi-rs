//! `pi-rs run --resume`: the flag surface, the paths that refuse to run, and a turn
//! appended to a recorded session.
//!
//! These tests drive the binary and check what the command leaves behind, because the
//! contract is about the store as much as about the message: a name that does not match
//! a recorded session must not be able to create one, a value that looks like a
//! flag must be refused while the command line is still all the command has read, and a
//! name that does match must gain a turn in the same session file rather than a new one.
//!
//! The refusals need no provider: their endpoint points at a port that was bound and
//! released, so any request would fail with a connection error rather than a resume
//! error, which is what makes "the run never reached the provider" visible. The one
//! continuation test answers from a fake provider, because only a request shows what the
//! model was actually given to continue from.

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
  ModelCapabilities, ModelEndpoint, ModelRef, ReasoningExposure, RuntimeConfig, SessionId,
};
use pi_rs_store::StateLayout;
use tempfile::TempDir;

/// A session id shape the store would list, used as the name of a session that exists.
const RECORDED: &str = "resume-fixture-0001";

/// An endpoint nobody is listening on.
fn closed_endpoint() -> String {
  let listener = TcpListener::bind("127.0.0.1:0").expect("bind an unused port");
  let address = listener.local_addr().expect("unused port address");
  drop(listener);
  format!("http://{address}/v1")
}

/// Config whose store root is `root/state`, which may or may not exist yet.
fn write_config(root: &Path) -> PathBuf {
  write_config_at(root, &closed_endpoint())
}

/// The same config answering from `base_url`, so a test can hold the store still and
/// move only the endpoint.
fn write_config_at(root: &Path, base_url: &str) -> PathBuf {
  let state = root.join("state");
  let mut config = RuntimeConfig::new(
    ModelRef::new("fake", "agent"),
    state.to_string_lossy().to_string(),
  );
  config.endpoints.push(ModelEndpoint {
    provider: "fake".into(),
    model: "agent".into(),
    base_url: Some(base_url.to_string()),
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
  fs::write(
    &path,
    serde_json::to_vec_pretty(&config).expect("serialize config"),
  )
  .expect("write config");
  path
}

/// Put one session in the store: the session file is what makes an id listable, and an
/// empty file is a session with no records, which is enough to name.
fn record_session(state: &Path, id: &str) -> PathBuf {
  let layout = StateLayout::new(state);
  fs::create_dir_all(layout.sessions_dir()).expect("create the sessions dir");
  let path = layout.session_path(&SessionId::from_string(id.to_string()));
  fs::write(&path, "").expect("write the session file");
  path
}

fn recorded_count(state: &Path) -> usize {
  StateLayout::new(state)
    .list_session_ids()
    .expect("list sessions")
    .len()
}

fn run(config: &Path, cwd: &Path, extra: &[&str]) -> Output {
  run_prompt(config, cwd, "continue this", extra)
}

fn run_prompt(config: &Path, cwd: &Path, prompt: &str, extra: &[&str]) -> Output {
  Command::new(env!("CARGO_BIN_EXE_pi-rs"))
    .args(["run", "--config"])
    .arg(config)
    .arg("--cwd")
    .arg(cwd)
    .arg("--prompt")
    .arg(prompt)
    .args(extra)
    .output()
    .expect("run pi-rs")
}

fn stderr(out: &Output) -> String {
  String::from_utf8_lossy(&out.stderr).to_string()
}

/// The session ids the store holds, as written text, so an id can be sliced into a
/// prefix and compared against the one that was named.
fn recorded_ids(state: &Path) -> Vec<String> {
  StateLayout::new(state)
    .list_session_ids()
    .expect("list sessions")
    .iter()
    .map(|id| id.as_str().to_string())
    .collect()
}

/// A provider that answers each request with the next scripted body and keeps the
/// request bytes, because the contract is about what the model was actually sent.
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
                assert!(Instant::now() < deadline, "timed out waiting for a request");
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

/// One complete assistant answer, as the OpenAI-compatible stream the runtime reads.
fn text_response(text: &str) -> String {
  let body = format!(
    "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
    serde_json::json!({ "choices": [{ "delta": { "content": text } }] }),
    serde_json::json!({
      "choices": [{ "delta": {}, "finish_reason": "stop" }],
      "usage": { "prompt_tokens": 20, "completion_tokens": 3 }
    }),
  );
  format!(
    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
    body.len()
  )
}

#[test]
fn a_resume_value_that_looks_like_a_flag_is_refused_before_anything_is_opened() {
  // The config path is deliberately absent: had the parser handed the flag-shaped
  // value to the runtime, the failure would be about reading a config file. Both the
  // spaced and the inline form must be caught while the command line is all that has
  // been read.
  let temp = TempDir::new().expect("temp dir");
  let absent = temp.path().join("absent-config.json");
  for flag in ["--resume", "--resume=-x"] {
    let extra = if flag == "--resume" {
      ["--resume", "--quiet"]
    } else {
      [flag, "ignored"]
    };
    let out = run(&absent, temp.path(), &extra);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    let message = stderr(&out);
    assert!(message.contains("--resume needs a session id"), "{message}");
    assert!(!message.contains("cannot read config"), "{message}");
  }
  // Nothing was opened, so nothing was created: the workspace stays empty.
  assert_eq!(fs::read_dir(temp.path()).expect("read temp dir").count(), 0);
}

#[test]
fn a_resume_value_that_is_absent_is_refused() {
  let temp = TempDir::new().expect("temp dir");
  let absent = temp.path().join("absent-config.json");
  let out = run(&absent, temp.path(), &["--resume"]);
  assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
  assert!(stderr(&out).contains("--resume requires a value"));
}

#[test]
fn an_unknown_session_id_is_refused_without_creating_a_session() {
  let temp = TempDir::new().expect("temp dir");
  let config = write_config(temp.path());
  let state = temp.path().join("state");
  let out = run(&config, temp.path(), &["--resume", "nosuchsession"]);
  assert!(!out.status.success(), "stderr: {}", stderr(&out));
  let message = stderr(&out);
  assert!(
    message.contains("no session id starts with 'nosuchsession'"),
    "{message}"
  );
  // The store root is created by the write path only. A name that matches nothing
  // must not leave a store behind, and must not have reached the endpoint.
  assert!(!state.exists(), "a store was created for an unknown id");
  assert!(!message.contains("connection"), "{message}");
}

#[test]
fn a_recorded_session_gains_a_second_turn_under_the_same_id() {
  // Continuing a session means two things at once: the turn is appended to the session
  // file that was named, and the model is actually given the earlier turn to continue
  // from. Both are checked from outside, because either half can be satisfied without
  // the other — a second file under a new id, or one file whose next request was built
  // from the prompt alone.
  let temp = TempDir::new().expect("temp dir");
  let workspace = temp.path().join("workspace");
  fs::create_dir(&workspace).expect("create the workspace");
  let state = temp.path().join("state");

  // The first turn is an ordinary run, so the session it creates is one the store
  // really holds rather than one written by hand.
  let first = FakeServer::answer(vec![text_response("the first answer")]);
  let config = write_config_at(temp.path(), &first.base_url());
  let out = run_prompt(&config, &workspace, "recite the old promise", &[]);
  assert!(out.status.success(), "stderr: {}", stderr(&out));
  assert_eq!(first.requests().len(), 1, "the first turn is one request");
  let recorded = recorded_ids(&state);
  assert_eq!(recorded.len(), 1, "the first turn creates one session");
  let session_id = &recorded[0];

  // The second turn names that session by a prefix, exactly as `pi-rs trace` would.
  let second = FakeServer::answer(vec![text_response("the second answer")]);
  let config = write_config_at(temp.path(), &second.base_url());
  let out = run_prompt(
    &config,
    &workspace,
    "keep the promise",
    &["--resume", &session_id[..8]],
  );
  assert!(out.status.success(), "stderr: {}", stderr(&out));

  // What the provider was actually sent, not what the runtime claims to remember.
  let requests = second.requests();
  assert_eq!(requests.len(), 1);
  let asked = &requests[0];
  assert!(asked.contains("recite the old promise"), "{asked}");
  assert!(asked.contains("the first answer"), "{asked}");
  assert!(asked.contains("keep the promise"), "{asked}");

  // Still one session, still the id that was named: a continuation is one session file.
  assert_eq!(
    recorded_ids(&state),
    vec![session_id.clone()],
    "resuming must not create a session"
  );
  assert_eq!(recorded_count(&state), 1);
  // Both turns are in the log that was appended to, in order.
  let log = fs::read_to_string(
    StateLayout::new(&state).session_path(&SessionId::from_string(session_id.clone())),
  )
  .expect("read the session log");
  let earlier = log
    .find("recite the old promise")
    .expect("the first turn is still recorded");
  let later = log
    .find("keep the promise")
    .expect("the resumed turn was appended");
  assert!(
    earlier < later,
    "the resumed turn is appended, not prepended"
  );
}

#[test]
fn a_session_whose_log_cannot_be_read_is_refused_without_creating_a_session() {
  // A named session whose log cannot be rebuilt is not continued with a partial context.
  // This fixture holds no header record at all, so the command names the session, says
  // what is missing, and writes nothing.
  let temp = TempDir::new().expect("temp dir");
  let config = write_config(temp.path());
  let state = temp.path().join("state");
  let session = record_session(&state, RECORDED);
  let before = fs::read(&session).expect("read the session file");

  // A prefix names the session, exactly as it does for `pi-rs trace`.
  let out = run(&config, temp.path(), &["--resume", "resume-fixture"]);
  assert!(!out.status.success(), "stderr: {}", stderr(&out));
  let message = stderr(&out);
  assert!(message.contains("cannot continue session"), "{message}");
  assert!(message.contains(RECORDED), "{message}");
  assert!(message.contains("no session header"), "{message}");
  // Refusal writes nothing: no second session, and the named one is untouched.
  assert_eq!(recorded_count(&state), 1);
  assert_eq!(fs::read(&session).expect("re-read"), before);
  // And it never reached the endpoint.
  assert!(!message.contains("connection"), "{message}");
}
