//! The fake provider is itself a claim about determinism, so it is pinned here.
//!
//! Two end-to-end binaries depend on this harness, and a regression in it does not look
//! like a harness bug: it looks like a flaky test somewhere else. Both properties that
//! the old copies lacked are asserted directly.

mod fake_provider;

use std::{
  io::{Read, Write},
  net::TcpStream,
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
