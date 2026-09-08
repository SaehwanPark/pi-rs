//! The fake provider is itself a claim about determinism, so it is pinned here.
//!
//! Two end-to-end binaries depend on this harness, and a regression in it does not look
//! like a harness bug: it looks like a flaky test somewhere else. Both properties that
//! the old copies lacked are asserted directly.

mod fake_provider;

use std::{
  io::{Read, Write},
  net::{Shutdown, TcpStream},
  thread,
  time::Duration,
};

use fake_provider::FakeServer;

fn post(authority: &str, body: &str) -> String {
  let mut socket = TcpStream::connect(authority).expect("connect the fake provider");
  let request = format!(
    "POST /v1/chat/completions HTTP/1.1\r\nhost: {authority}\r\ncontent-length: {}\r\n\r\n{body}",
    body.len()
  );
  socket.write_all(request.as_bytes()).expect("write request");
  socket.flush().expect("flush request");
  let mut answer = Vec::new();
  let _ = socket.read_to_end(&mut answer);
  String::from_utf8_lossy(&answer).into_owned()
}

/// The listener outlives the script, and going past the script is a defined answer
/// rather than a closed port. A refused connection is invisible in the record, which is
/// exactly how a retry once went unnoticed and a request count came to look smaller than
/// the attempts.
#[test]
fn every_request_is_answered_and_recorded() {
  let server = FakeServer::answer(vec!["HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n".into()]);
  let authority = server.addr_authority();
  let first = post(&authority, "{}");
  let second = post(&authority, "{}");
  let third = post(&authority, "{}");
  assert!(first.contains("200 OK"), "{first}");
  assert!(
    second.contains("503 Script Exhausted"),
    "past the script the answer is distinguishable and still classified as unavailable: \
     {second}"
  );
  assert!(third.contains("503 Script Exhausted"), "{third}");
}

/// A scripted answer that no request asked for is a test that no longer describes the
/// run. `requests` says so, with the request lines that did arrive, instead of waiting
/// on a wall clock in a thread whose panic mentions neither.
#[test]
#[should_panic(expected = "1 scripted answer, 3 requests recorded")]
fn a_script_that_does_not_match_the_requests_fails_loudly() {
  let server = FakeServer::answer(vec!["HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n".into()]);
  let authority = server.addr_authority();
  for _ in 0..3 {
    post(&authority, "{}");
  }
  let observed = server.requests();
  assert_eq!(observed.len(), 1);
}

/// A connection the client abandons is not a request, and must not be answered from the
/// script. It used to be, which is how one transport error on macOS cost the run its
/// scripted reasoning answer while still printing an answer — and the assertion about
/// provenance failed two turns later, somewhere that looked nothing like the cause.
#[test]
fn an_abandoned_connection_does_not_cost_a_scripted_answer() {
  let server = FakeServer::answer(vec![
    "HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nfirst".into(),
    "HTTP/1.1 200 OK\r\ncontent-length: 6\r\n\r\nsecond".into(),
  ]);
  let authority = server.addr_authority();

  // A connection that says nothing: a probe, or a client that failed before writing.
  let probe = TcpStream::connect(&authority).expect("connect to abandon");
  drop(probe);

  // A connection that starts a request and breaks off before the body arrives.
  let mut half = TcpStream::connect(&authority).expect("connect a half request");
  half
    .write_all(b"POST /v1/chat/completions HTTP/1.1\r\ncontent-length: 4096\r\n\r\n{\"partial\":")
    .expect("write a half request");
  half.shutdown(Shutdown::Write).expect("hang up");
  drop(half);

  // The requests the run really makes are answered from the start of the script, in
  // order, and only those two are recorded.
  let first = post(&authority, "{}");
  let second = post(&authority, "{}");
  assert!(
    first.contains("first"),
    "the first answer was spent elsewhere: {first}"
  );
  assert!(second.contains("second"), "the script shifted: {second}");
  let observed = server.requests();
  assert_eq!(observed.len(), 2, "only complete requests are requests");
}

/// A request that arrives in pieces is still one request. BSD and macOS hand the accepted
/// socket the listener's non-blocking flag, where a short read answers `WouldBlock`;
/// reading that as the end of the request is how a scripted answer once went missing on
/// macOS under load, while Linux — which does not inherit the flag — stayed green.
#[test]
fn a_request_that_arrives_in_pieces_is_answered_once() {
  let server = FakeServer::answer(vec![
    "HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok".into(),
  ]);
  let authority = server.addr_authority();
  let mut socket = TcpStream::connect(&authority).expect("connect the fake provider");

  let body = "{\"seg\":1}";
  let request = format!(
    "POST /v1/chat/completions HTTP/1.1\r\nhost: {authority}\r\ncontent-length: {}\r\n\r\n{body}",
    body.len()
  );
  let (head, tail) = request.split_at(24);
  socket
    .write_all(head.as_bytes())
    .expect("write the first piece");
  socket.flush().expect("flush the first piece");
  thread::sleep(Duration::from_millis(120));
  socket
    .write_all(tail.as_bytes())
    .expect("write the second piece");
  socket.flush().expect("flush the second piece");

  let mut raw = Vec::new();
  let _ = socket.read_to_end(&mut raw);
  let answer = String::from_utf8_lossy(&raw).into_owned();
  assert!(
    answer.contains("200 OK"),
    "a segmented request should be answered, not dropped: {answer}"
  );
  let observed = server.requests();
  assert_eq!(observed.len(), 1);
  assert_eq!(observed[0].body, body, "the body must be reassembled");
}
