//! A deterministic fake OpenAI-compatible provider, shared by the end-to-end tests.
//!
//! `run_cli` and `failover_cli` each carried a copy of this, and the copies had drifted
//! into the same two failure modes — one of which was a real flake on shared runners:
//!
//! * The listener was dropped as soon as the script had been served. A connection that
//!   arrived after that was refused, and a refused connection was never recorded. So
//!   whether a retry landed in the window before the accept thread exited decided the
//!   recorded request count, and a transport hiccup on the *first* request silently
//!   shifted the whole script: the run then saw the second scripted answer instead of
//!   the first, still printed the expected answer, and the test failed much later on a
//!   completely unrelated assertion.
//! * Each scripted answer that was never asked about carried a five-second wall-clock
//!   deadline asserted inside the accept thread. A loaded runner could therefore fail a
//!   test for being slow rather than for being wrong, and the panic surfaced in a worker
//!   thread with no reference to the run it was waiting for.
//!
//! The rules here are the opposite, and each one exists so that a failure names itself:
//!
//! * The listener lives until the `FakeServer` is dropped, so every request the runtime
//!   actually makes is recorded. Once the script is exhausted the server keeps answering with a
//!   distinguishable `503`, which the provider classifies like any other unavailable
//!   response. An unexpected extra attempt therefore shows up in [`FakeServer::requests`]
//!   instead of being swallowed by a closed port.
//! * A scripted answer that was never asked about is reported by [`FakeServer::requests`]
//!   in the test thread, alongside the request lines that did arrive. That is the same
//!   claim the wall-clock deadline made, without the race against the scheduler.
//! * Reads carry a timeout and a byte bound, so a half-open connection can neither wedge
//!   the accept loop nor grow it without limit, and [`FakeServer::requests`] always
//!   returns.
//! * Only a *complete* request consumes a scripted answer. The listener accepts before it
//!   knows what will arrive, and a connection that breaks off — a client that failed to
//!   connect cleanly, a probe, a transport error mid-write — used to be answered from the
//!   script anyway. That is how one `os error 22` on macOS cost the run its scripted
//!   reasoning answer, still printed an answer, and failed an unrelated assertion about
//!   provenance two turns later. An abandoned connection is dropped instead; one that
//!   sent bytes is named on stderr, because "the client broke mid-request" is worth
//!   knowing when the trace is about to show a retry.

// Each end-to-end binary uses part of this module: `failover_cli` never builds a tool
// call with a reasoning delta, `run_cli` never asks for a backup. Neither binary is
// wrong about that, so the shared module is allowed to carry what only the other needs.
#![allow(dead_code)]

use std::{
  fs,
  io::{Read, Write},
  net::{Shutdown, SocketAddr, TcpListener, TcpStream},
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
  thread,
  time::{Duration, Instant},
};

/// How long a request may take to finish arriving before the fake answers what it has.
/// Generous on purpose: this is a bound against a wedged connection, not a latency claim.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Poll interval of the non-blocking accept loop, and how quickly a `FakeServer` that
/// is being dropped stops the thread.
const POLL: Duration = Duration::from_millis(2);

/// Refuse to accumulate more than this for one request. A conversation is small in
/// these tests; the bound exists so that a bug cannot turn a test into a memory leak.
const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;

/// One request as the fake provider saw it.
pub struct Observed {
  /// The request line, for example `POST /v1/chat/completions HTTP/1.1`.
  pub line: String,
  /// The body, which is where a conversation is checked.
  pub body: String,
}

/// A provider that answers `POST /v1/chat/completions` from a script.
pub struct FakeServer {
  addr: SocketAddr,
  /// Number of scripted answers: the request count the run owed this endpoint.
  scripted: usize,
  stop: Arc<AtomicBool>,
  handle: Option<thread::JoinHandle<Vec<Observed>>>,
}

impl FakeServer {
  /// Serves `responses`, one per request, in order, and records every request.
  pub fn answer(responses: Vec<String>) -> Self {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake provider");
    let addr = listener.local_addr().expect("fake provider address");
    listener.set_nonblocking(true).expect("non-blocking accept");
    let scripted = responses.len();
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = Arc::clone(&stop);
    let handle = thread::spawn(move || {
      let mut observed = Vec::new();
      let mut script = responses.into_iter();
      loop {
        if stopping.load(Ordering::Relaxed) {
          break;
        }
        match listener.accept() {
          Ok((mut socket, _)) => {
            // BSD and macOS hand the accepted socket the listener's `O_NONBLOCK`; Linux
            // does not. A non-blocking socket ignores `SO_RCVTIMEO`, so on macOS a short
            // read under load answers `WouldBlock`, and this fixture read that as the end
            // of the request: it lost bytes, and then lost a scripted answer with them.
            // Make the accepted socket blocking, so the read timeout means the same thing
            // on both platforms.
            let _ = socket.set_nonblocking(false);
            let _ = socket.set_read_timeout(Some(READ_TIMEOUT));
            match drain_request(&mut socket) {
              Drain::Complete(request) => {
                // Past the script the answer is still a defined one, and it is still
                // classified as the provider being unavailable, so the runtime's recovery
                // path is unchanged by the fixture having run out of material.
                let answer = script
                  .next()
                  .unwrap_or_else(|| exhausted_response(&request.line));
                // The runtime may already have hung up. That is not a failure of the
                // fake: the request is recorded either way, and `requests` reports the
                // script.
                let _ = socket.write_all(answer.as_bytes());
                let _ = socket.shutdown(Shutdown::Write);
                observed.push(request);
              }
              // A connection that never completed a request is not a request, and giving
              // it a scripted answer is how one transport hiccup silently shifted the
              // whole script. A connection that sent *something* is worth a line: it
              // means the client broke mid-request, and the run's retry is about to look
              // mysterious in the trace without it.
              Drain::Partial(line) => {
                eprintln!("[fake-provider] dropped an incomplete request: {line}");
              }
              Drain::Silent => {}
            }
          }
          Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(POLL),
          Err(error) => panic!("accept provider request: {error}"),
        }
      }
      observed
    });
    Self {
      addr,
      scripted,
      stop,
      handle: Some(handle),
    }
  }

  /// Host and port, for a test that speaks HTTP to the fake directly.
  pub fn addr_authority(&self) -> String {
    self.addr.to_string()
  }

  /// Value for a `ModelEndpoint::base_url`. The `/v1` suffix matters: the adapter
  /// appends `/chat/completions` to it, so an endpoint without a path is addressed wrong.
  pub fn base_url(&self) -> String {
    format!("http://{}/v1", self.addr)
  }

  /// Stop the accept loop, join it, and hand back the recorded requests.
  ///
  /// Requiring the script to have been consumed is the point of the join. A scripted
  /// answer that no request ever asked for means the run did not do what the test
  /// assumed, and that is worth saying where the reader can see the requests.
  pub fn requests(mut self) -> Vec<Observed> {
    self.stop.store(true, Ordering::Relaxed);
    let handle = self.handle.take().expect("accept thread");
    let observed = handle.join().expect("fake provider thread");
    if observed.len() != self.scripted {
      let lines: Vec<&str> = observed
        .iter()
        .map(|request| request.line.as_str())
        .collect();
      let noun = if self.scripted == 1 {
        "answer"
      } else {
        "answers"
      };
      panic!(
        "{} scripted {noun}, {} request{} recorded: {lines:?}",
        self.scripted,
        observed.len(),
        if observed.len() == 1 { "" } else { "s" }
      );
    }
    observed
  }
}

impl Drop for FakeServer {
  /// A test that never asked for the requests still must not leave a thread accepting
  /// connections for the life of the test process.
  fn drop(&mut self) {
    self.stop.store(true, Ordering::Relaxed);
  }
}

/// The answer given once the script is exhausted. Distinct body so that a run which
/// asked one time too many can be told apart from one that asked the right number.
fn exhausted_response(request_line: &str) -> String {
  // The line is quoted back so a failure says which endpoint ran out, not only that one
  // did. It is a request line, so it carries no credential.
  let body =
    format!("{{\"error\":{{\"message\":\"fake provider script exhausted by {request_line}\"}}}}");
  status_response(503, "Script Exhausted", &body)
}

/// Read one HTTP request, stopping at the declared body length, at end of stream, or at
/// the bound. The request line and body are separated because assertions want the body.
/// What arrived on one accepted connection.
enum Drain {
  /// A request line, headers, and the body those headers promised.
  Complete(Observed),
  /// Bytes arrived, but not a whole request: the client broke off, or the read bound
  /// expired. Carries the request line so the note is not anonymous.
  Partial(String),
  /// The connection was accepted and said nothing at all: a port probe, or a client that
  /// gave up before writing. Not worth a line of its own.
  Silent,
}

fn drain_request(socket: &mut TcpStream) -> Drain {
  let mut bytes = Vec::new();
  let mut buffer = [0u8; 4096];
  let mut expected = None;
  let deadline = Instant::now() + READ_TIMEOUT;
  loop {
    let read = match socket.read(&mut buffer) {
      Ok(read) => read,
      // `WouldBlock` is not the end of a request, and an interrupted read is not
      // either. Only EOF or a real error ends the read early, and a socket that somehow
      // stayed non-blocking is still bounded by the deadline below.
      Err(error)
        if matches!(
          error.kind(),
          std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
        ) =>
      {
        if Instant::now() >= deadline {
          break;
        }
        thread::sleep(POLL);
        continue;
      }
      Err(_) => break,
    };
    if read == 0 || bytes.len() >= MAX_REQUEST_BYTES {
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
  let first_line = || {
    let line = text.lines().next().unwrap_or("").trim();
    if line.is_empty() {
      format!("{} bytes, no request line", bytes.len())
    } else {
      line.to_string()
    }
  };
  match expected {
    Some(length) if bytes.len() >= length => {
      let body = text[text.rfind("\r\n\r\n").unwrap() + 4..].to_string();
      let line = text.lines().next().unwrap_or("POST (unread)").to_string();
      Drain::Complete(Observed { line, body })
    }
    // Headers without the body they promised, or no headers at all: either way the
    // client did not finish, so there is no request to answer.
    _ => Drain::Partial(first_line()),
  }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
  haystack
    .windows(needle.len())
    .position(|part| part == needle)
}

/// A streaming completion: each `event` is one `data:` frame, then `[DONE]`.
pub fn sse(events: &[serde_json::Value]) -> String {
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

/// A completion that says `text` and stops.
pub fn text_response(text: &str) -> String {
  sse(&[
    serde_json::json!({"choices": [{"delta": {"content": text}}]}),
    serde_json::json!({
      "choices": [{"delta": {}, "finish_reason": "stop"}],
      "usage": {"prompt_tokens": 20, "completion_tokens": 3}
    }),
  ])
}

/// A non-success status with a small body.
pub fn status_response(code: u16, reason: &str, body: &str) -> String {
  format!(
    "HTTP/1.1 {code} {reason}\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
    body.len()
  )
}

/// A response that asks for a tool, then ends the stream.
pub fn tool_call(id: &str, name: &str, arguments: &str) -> String {
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

/// An endpoint that is up but refusing work: classified as an availability failure,
/// which is what a failover test needs the runtime to react to.
pub fn unavailable() -> String {
  status_response(503, "Service Unavailable", r#"{"error":"down"}"#)
}

/// A file the run is expected to have written, read with a message that says what the
/// fake provider was asked. A missing side effect is almost always a shifted script, and
/// the request bodies are what tell the two apart.
pub fn read_written(path: &std::path::Path, requests: &[Observed]) -> Vec<u8> {
  let asked: Vec<&str> = requests
    .iter()
    .map(|request| request.line.as_str())
    .collect();
  match fs::read(path) {
    Ok(bytes) => bytes,
    Err(error) => panic!(
      "no '{}' after the run: {error}; the provider was asked: {asked:?}",
      path.display()
    ),
  }
}
