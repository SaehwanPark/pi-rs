//! Wire-level integration tests against a fake OpenAI-compatible server.
//!
//! These exercise the parts unit tests cannot reach: the actual HTTP request,
//! header construction, SSE decoding over a socket, and the phase and
//! `partial_output_emitted` of a failure observed through the provider trait.
//! It uses a plain `TcpListener` rather than an HTTP framework so the harness
//! gains no dev-dependency and the server stays small enough to trust.

use std::{
  io::{Read, Write},
  net::{SocketAddr, TcpListener, TcpStream},
  thread,
};

use pi_rs_core::{
  CancelToken, Collector, CompletionCertainty, CompletionUsage, FailurePhase, Message,
  ModelCapabilities, ModelFailure, ModelFailureKind, ModelProvider, ModelRef, ModelRequest,
  ProviderEvent, ReasoningExposure, ReasoningProvenance,
};
use pi_rs_provider::{MaxTokensField, OpenAiCompat, ProviderConfig, ThinkingInput};

/// A one-shot HTTP server that answers a single request with raw bytes.
struct FakeServer {
  addr: SocketAddr,
  handle: Option<thread::JoinHandle<String>>,
}

impl FakeServer {
  fn answer(raw_response: impl Into<String> + Send + 'static) -> Self {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let raw = raw_response.into();
    let handle = thread::spawn(move || {
      let (mut socket, _) = listener.accept().expect("accept");
      let request = drain_request(&mut socket);
      let _ = socket.write_all(raw.as_bytes());
      let _ = socket.flush();
      let _ = socket.shutdown(std::net::Shutdown::Write);
      request
    });
    Self {
      addr,
      handle: Some(handle),
    }
  }

  /// The request text the server saw, which is what most of these tests
  /// actually assert about.
  fn request(self) -> String {
    self.handle.expect("handle").join().expect("server thread")
  }

  fn base_url(&self) -> String {
    format!("http://{}/v1", self.addr)
  }
}

fn drain_request(socket: &mut TcpStream) -> String {
  let mut bytes = Vec::new();
  let mut buf = [0u8; 1024];
  let mut header_end = None;
  let mut expected = 0usize;
  loop {
    let read = socket.read(&mut buf).unwrap_or(0);
    if read == 0 {
      break;
    }
    bytes.extend_from_slice(&buf[..read]);
    if header_end.is_none() {
      header_end = find(&bytes, b"\r\n\r\n").map(|i| i + 4);
      if let Some(end) = header_end {
        let headers = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
        let length: usize = headers
          .split("\r\n")
          .find_map(|line| line.strip_prefix("content-length:"))
          .and_then(|value| value.trim().parse().ok())
          .unwrap_or(0);
        expected = end + length;
        if bytes.len() >= expected {
          break;
        }
        continue;
      }
    }
    if let Some(end) = header_end
      && bytes.len() >= expected.max(end)
    {
      break;
    }
  }
  String::from_utf8_lossy(&bytes).into_owned()
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
  haystack.windows(needle.len()).position(|w| w == needle)
}

/// A body that is a *complete* completion: without a finish reason the adapter
/// must report an interrupted turn, so wire-inspection tests use this rather
/// than a bare `[DONE]`.
fn complete_sse(body: &str) -> String {
  sse(format!(
    "{body}\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
  ))
}

fn sse(body: impl AsRef<str>) -> String {
  format!(
    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{}",
    body.as_ref()
  )
}

fn status(code: u16, reason: &str, body: &str, extra: &str) -> String {
  format!(
    "HTTP/1.1 {code} {reason}\r\ncontent-type: application/json\r\nconnection: close\r\n{extra}content-length: {}\r\n\r\n{body}",
    body.len()
  )
}

fn adapter(base_url: &str, api_key: Option<&str>) -> OpenAiCompat {
  adapter_declaring(base_url, api_key, ReasoningExposure::Native)
}

/// An endpoint that declares a specific reasoning exposure.
///
/// The declaration is the only evidence available about thinking text, so a test
/// about provenance has to state it.
fn adapter_declaring(
  base_url: &str,
  api_key: Option<&str>,
  exposure: ReasoningExposure,
) -> OpenAiCompat {
  let config = ProviderConfig {
    api_key: api_key.map(str::to_string),
    max_tokens_field: MaxTokensField::MaxCompletionTokens,
    thinking_input: ThinkingInput::ChatTemplateThinking,
    capabilities: ModelCapabilities {
      context_window: 8_192,
      exposed_reasoning: exposure,
      tools: true,
      ..ModelCapabilities::text_only(1)
    },
    ..ProviderConfig::local("local-vulkan", "qwen3.8-flash", base_url, 8_192)
  };
  OpenAiCompat::new(config).expect("adapter")
}

fn request(text: &str) -> ModelRequest {
  let mut request = ModelRequest::new(
    ModelRef::new("local-vulkan", "qwen3.8-flash"),
    ModelCapabilities {
      tools: true,
      ..ModelCapabilities::text_only(1)
    },
    vec![Message::system("be brief"), Message::user(text)],
  );
  request.max_output_tokens = Some(256);
  request
}

fn stream(
  adapter: &OpenAiCompat,
  req: &ModelRequest,
) -> (Result<CompletionUsage, ModelFailure>, Collector) {
  let mut collector = Collector::default();
  let result = adapter.stream(req, &mut collector, &CancelToken::new());
  (result, collector)
}

#[test]
fn a_streaming_request_is_a_well_formed_openai_post() {
  let server = FakeServer::answer(sse(
    "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"index\":0}]}\n\
     \n\
     data: [DONE]\n\n",
  ));
  let adapter = adapter(&server.base_url(), Some("sk-local"));
  let (result, collector) = stream(&adapter, &request("hello"));
  let usage = result.expect("completion");
  let request_text = server.request();

  assert!(
    request_text.starts_with("POST /v1/chat/completions HTTP/1.1"),
    "{request_text}"
  );
  assert!(
    request_text
      .to_lowercase()
      .contains("authorization: bearer sk-local")
  );
  assert!(
    request_text
      .to_lowercase()
      .contains("content-type: application/json")
  );
  let body_start = request_text.find("\r\n\r\n").expect("body");
  let body: serde_json::Value =
    serde_json::from_str(&request_text[body_start + 4..]).expect("json body");
  assert_eq!(body["model"], "qwen3.8-flash");
  assert_eq!(body["stream"], true);
  assert_eq!(body["stream_options"]["include_usage"], true);
  assert_eq!(
    body["max_completion_tokens"], 256,
    "dialect switch is honoured"
  );
  assert_eq!(body["messages"][0]["role"], "system");
  assert_eq!(usage.finish_reason, None, "no finish_reason on this stream");
  let events = collector.events();
  assert!(
    matches!(&events[0], ProviderEvent::TextDelta(t) if t == "hi"),
    "{events:?}"
  );
}

#[test]
fn reasoning_and_text_arrive_as_separate_typed_events() {
  let server = FakeServer::answer(sse(
    "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"step one \"}}]}\n\n\
     data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"step two\"}}]}\n\n\
     data: {\"choices\":[{\"delta\":{\"content\":\"answer\"}}]}\n\n\
     data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":5}}\n\n\
     data: [DONE]\n\n",
  ));
  let adapter = adapter(&server.base_url(), None);
  let (result, collector) = stream(&adapter, &request("why"));
  let usage = result.expect("completion");

  let events = collector.events();
  assert_eq!(events.len(), 3, "{events:?}");
  assert!(
    matches!(
      &events[0],
      ProviderEvent::ReasoningDelta { text, .. } if text == "step one "
    ),
    "{events:?}"
  );
  assert!(matches!(&events[1], ProviderEvent::ReasoningDelta { .. }));
  assert!(matches!(&events[2], ProviderEvent::TextDelta(t) if t == "answer"));
  assert_eq!(usage.input_tokens, Some(12));
  assert_eq!(usage.output_tokens, Some(5));
  assert_eq!(usage.finish_reason.as_deref(), Some("stop"));

  // The thinking text is never merged into the visible answer.
  let text: String = events
    .iter()
    .filter_map(|event| match event {
      ProviderEvent::TextDelta(t) => Some(t.as_str()),
      _ => None,
    })
    .collect();
  assert_eq!(text, "answer");
}

/// The same thinking field, three different claims.
///
/// A hosted endpoint that exposes only a summary of hidden reasoning sends that
/// summary in `reasoning_content`, the field a local server uses for the model's own
/// thinking. Deciding by field name would record provider prose as recovered chain of
/// thought, so the endpoint's declaration decides, and the claim survives the wire.
#[test]
fn thinking_text_is_claimed_as_the_declaration_says() {
  let thinking = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"in short\"}}]}\n\n";
  for (exposure, expected) in [
    (ReasoningExposure::Native, ReasoningProvenance::Native),
    (
      ReasoningExposure::ProviderSummary,
      ReasoningProvenance::ProviderSummary,
    ),
    (ReasoningExposure::Declared, ReasoningProvenance::Declared),
  ] {
    let server = FakeServer::answer(complete_sse(thinking));
    let adapter = adapter_declaring(&server.base_url(), None, exposure);
    let (result, collector) = stream(&adapter, &request("why"));
    result.expect("completion");
    let events = collector.events();
    assert!(
      matches!(
        &events[0],
        ProviderEvent::ReasoningDelta { text, provenance }
          if text == "in short" && *provenance == expected
      ),
      "{exposure:?} => {events:?}"
    );
  }
}

#[test]
fn a_tool_call_completes_only_at_finish() {
  let server = FakeServer::answer(sse(
    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\"}\"}}]}}]}\n\n\
     data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
     data: [DONE]\n\n",
  ));
  let adapter = adapter(&server.base_url(), None);
  let (result, collector) = stream(&adapter, &request("read it"));
  result.expect("completion");

  let events = collector.events();
  assert_eq!(
    events.len(),
    1,
    "a fragment must not surface as a call: {events:?}"
  );
  match &events[0] {
    ProviderEvent::ToolCall(call) => {
      assert_eq!(call.name, "read");
      assert_eq!(call.arguments["path"], "a.rs");
    }
    other => panic!("expected a tool call, got {other:?}"),
  }
}

#[test]
fn a_400_context_overflow_is_classified_before_the_status_code() {
  let server = FakeServer::answer(status(
    400,
    "Bad Request",
    r#"{"error":{"message":"Requested context of 200000 tokens exceeds supported context size 8192"}}"#,
    "",
  ));
  let adapter = adapter(&server.base_url(), None);
  let (result, _) = stream(&adapter, &request("too big"));
  let failure = result.unwrap_err();
  assert_eq!(failure.kind, ModelFailureKind::ContextOverflow);
  assert_eq!(failure.phase, FailurePhase::WaitingForResponse);
  assert_eq!(failure.status, Some(400));
  assert!(!failure.safe_to_retry(), "{failure:?}");
  server.request();
}

#[test]
fn rate_limiting_carries_its_retry_hint() {
  let server = FakeServer::answer(status(429, "Too Many Requests", "{}", "retry-after: 7\r\n"));
  let adapter = adapter(&server.base_url(), None);
  let (result, _) = stream(&adapter, &request("slow down"));
  let failure = result.unwrap_err();
  assert_eq!(failure.kind, ModelFailureKind::RateLimited);
  assert_eq!(failure.retry_after_ms, Some(7_000));
  assert!(failure.safe_to_retry());
  server.request();
}

#[test]
fn authentication_failures_are_not_retryable() {
  let server = FakeServer::answer(status(
    401,
    "Unauthorized",
    r#"{"error":{"message":"bad key"}}"#,
    "",
  ));
  let adapter = adapter(&server.base_url(), None);
  let (result, _) = stream(&adapter, &request("nope"));
  let failure = result.unwrap_err();
  assert_eq!(failure.kind, ModelFailureKind::Authentication);
  assert!(!failure.safe_to_retry());
  assert_eq!(failure.status, Some(401));
  server.request();
}

#[test]
fn a_stream_that_reports_nothing_is_not_a_completion() {
  // `[DONE]` alone is an end of connection, not a claim of completion. Without
  // a finish reason and without output, treating it as success would let the
  // runtime record an empty assistant turn as if the model had answered.
  let server = FakeServer::answer(sse("data: [DONE]\n\n"));
  let adapter = adapter(&server.base_url(), None);
  let (result, collector) = stream(&adapter, &request("say nothing"));
  let failure = result.unwrap_err();
  assert_eq!(failure.kind, ModelFailureKind::Protocol);
  assert!(!failure.partial_output_emitted);
  assert!(collector.events().is_empty());
  server.request();
}

#[test]
fn a_mid_stream_disconnect_reports_an_uncertain_completion() {
  // Headers and one delta, then the socket closes without DONE and without a
  // finish_reason: exactly the mid-turn case the roadmap requires coverage for.
  let server = FakeServer::answer(sse(
    "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n",
  ));
  let adapter = adapter(&server.base_url(), None);
  let (result, collector) = stream(&adapter, &request("stream then die"));
  let usage = result.expect("a truncated stream is a fact, not a transport fault");

  // Two separate facts, kept separate: the connection ended, and the provider never
  // said it was finished. Collapsing them into a transport error would let a
  // runtime treat "retry" as safe when a delta is already committed content.
  assert_eq!(usage.certainty, CompletionCertainty::Unknown);
  let events = collector.events();
  assert_eq!(events.len(), 1);
  assert!(
    !usage.is_certain(),
    "the runtime must be able to see the turn did not finish"
  );
  server.request();
}

#[test]
fn a_decoded_tool_call_without_done_is_an_uncertain_completion() {
  // A complete tool call is still unsafe to execute when the socket closes
  // before the provider's stream sentinel arrives.
  let server = FakeServer::answer(sse(
    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_uncertain\",\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"path\\\":\\\"out.txt\\\",\\\"contents\\\":\\\"x\\\"}\"}}]}}]}\n\n",
  ));
  let adapter = adapter(&server.base_url(), None);
  let (result, collector) = stream(&adapter, &request("write it"));
  let usage = result.expect("the decoded call has an uncertain boundary");

  assert_eq!(usage.certainty, CompletionCertainty::Unknown);
  assert_eq!(usage.finish_reason, None);
  assert!(matches!(
    collector.events().first(),
    Some(ProviderEvent::ToolCall(call)) if call.name == "write"
  ));
  server.request();
}

#[test]
fn a_non_streaming_endpoint_is_supported() {
  let server = FakeServer::answer(status(
    200,
    "OK",
    r#"{"choices":[{"message":{"role":"assistant","content":"one shot","reasoning_content":"thoughts"},"finish_reason":"stop"}],"usage":{"prompt_tokens":4,"completion_tokens":3}}"#,
    "",
  ));
  let mut config = ProviderConfig::local("local-vulkan", "qwen3.8-flash", server.base_url(), 8_192);
  config.stream = false;
  let adapter = OpenAiCompat::new(config).expect("adapter");
  let (result, collector) = stream(&adapter, &request("hello"));
  let usage = result.expect("completion");
  assert_eq!(usage.finish_reason.as_deref(), Some("stop"));
  let events = collector.events();
  assert!(
    matches!(&events[0], ProviderEvent::ReasoningDelta { .. }),
    "{events:?}"
  );
  assert!(matches!(&events[1], ProviderEvent::TextDelta(t) if t == "one shot"));
  let request_text = server.request();
  assert!(request_text.contains(r#""stream":false"#));
}

#[test]
fn thinking_is_disabled_with_the_configured_template_dialect() {
  let server = FakeServer::answer(complete_sse(""));
  let adapter = adapter(&server.base_url(), None);
  let mut req = request("calm down");
  req.thinking = pi_rs_core::ThinkingLevel::Off;
  let (result, _) = stream(&adapter, &req);
  result.expect("completion");
  let request_text = server.request();
  assert!(
    request_text.contains(r#""chat_template_kwargs""#),
    "{request_text}"
  );
  assert!(
    request_text.contains(r#""thinking":false"#),
    "{request_text}"
  );
}

#[test]
fn tools_are_not_sent_when_the_model_cannot_use_them() {
  let server = FakeServer::answer(complete_sse(""));
  let adapter = adapter(&server.base_url(), None);
  let mut req = request("no tools");
  req.capabilities.tools = false;
  req.tools = vec![pi_rs_core::ToolSpec {
    name: "read".into(),
    description: "Read".into(),
    parameters: serde_json::json!({"type": "object"}),
  }];
  let (result, _) = stream(&adapter, &req);
  result.expect("completion");
  let request_text = server.request();
  assert!(!request_text.contains("\"tools\""), "{request_text}");
  assert!(!request_text.contains("tool_choice"), "{request_text}");
}

#[test]
fn an_injected_non_json_payload_after_output_does_not_destroy_the_answer() {
  let server = FakeServer::answer(sse(
    "data: {\"choices\":[{\"delta\":{\"content\":\"usable\"}}]}\n\n\
     data: <html>proxy injected</html>\n\n\
     data: {\"choices\":[{\"delta\":{\"content\":\" text\"}}]},{\"x\":[{\"delta\":{\"content\":\" tail\"}}]}\n\n",
  ));
  let adapter = adapter(&server.base_url(), None);
  // A garbage frame *after* output is skipped rather than fatal; the stream
  // then ends without a finish_reason, which is still an interrupted turn.
  let (result, collector) = stream(&adapter, &request("noisy proxy"));
  let events = collector.events();
  assert!(
    events
      .iter()
      .any(|event| matches!(event, ProviderEvent::TextDelta(t) if t == "usable")),
    "{events:?}"
  );
  assert!(result.is_err(), "an unterminated stream is still a failure");
  server.request();
}

#[test]
fn cancel_during_a_slow_stream_stops_promptly() {
  // The server sends one delta and then holds the connection open; the client
  // must abandon it on cancel rather than wait for a timeout.
  let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
  let addr = listener.local_addr().expect("addr");
  let server = thread::spawn(move || {
    let (mut socket, _) = listener.accept().expect("accept");
    let mut buf = [0u8; 512];
    let _ = socket.read(&mut buf);
    let _ = socket.write_all(
      b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"first\"}}]}\n\n",
    );
    let _ = socket.flush();
    // Keep the connection open until the client goes away.
    thread::sleep(std::time::Duration::from_secs(3));
  });

  let adapter = adapter(&format!("http://{addr}/v1"), None);
  let cancel = CancelToken::new();
  let mut collector = Collector::default();
  let thread_cancel = cancel.clone();
  let killer = thread::spawn(move || {
    thread::sleep(std::time::Duration::from_millis(150));
    thread_cancel.cancel();
  });
  let result = adapter.stream(&request("hang"), &mut collector, &cancel);
  killer.join().expect("killer");
  let _ = server.join();

  let failure = result.unwrap_err();
  assert_eq!(failure.kind, ModelFailureKind::Cancelled);
  assert_eq!(failure.phase, FailurePhase::Streaming);
  assert!(failure.partial_output_emitted);
  assert_eq!(collector.events().len(), 1);
}

#[test]
fn request_bodies_are_what_the_server_actually_received() {
  // Proves the mapping is not merely unit-test-local: the serialized bytes on
  // the wire carry the system prompt, the tool schema, and the model id.
  let server = FakeServer::answer(complete_sse(""));
  let adapter = adapter(&server.base_url(), None);
  let mut req = request("wire");
  req.tools = vec![pi_rs_core::ToolSpec {
    name: "grep".into(),
    description: "Search".into(),
    parameters: serde_json::json!({"type": "object", "properties": {"pattern": {"type": "string"}}}),
  }];
  req.stop = vec!["\n\nuser:".into()];
  let (result, _) = stream(&adapter, &req);
  result.expect("completion");
  let text = server.request();
  let body: serde_json::Value =
    serde_json::from_str(&text[text.find("\r\n\r\n").expect("body") + 4..]).expect("json");
  assert_eq!(body["messages"][0]["content"], "be brief");
  assert_eq!(body["tools"][0]["function"]["name"], "grep");
  assert_eq!(body["stop"][0], "\n\nuser:");
}
