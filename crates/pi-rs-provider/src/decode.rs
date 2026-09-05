//! Response decoding and failure normalization.
//!
//! Two responsibilities belong together here because they share the same
//! evidence: what has already been handed to the caller. A mid-stream failure
//! after visible output is a *different* failure than one before any output —
//! the runtime may retry the first and must not silently replay the second. The
//! decoder is therefore the only place that knows, honestly, whether output was
//! emitted.

use std::io;

use pi_rs_core::{
  CancelToken, CompletionCertainty, FailurePhase, ProviderEvent, ProviderEventSink,
  ReasoningProvenance, ToolCallBlock, ToolCallId, provider::CompletionUsage,
};
use serde_json::Value;

use crate::config::MAX_ERROR_BODY_BYTES;

/// How a response body ended, which decides whether the turn may be reported
/// as complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamEnd {
  /// The provider sent the `[DONE]` sentinel.
  DoneSentinel,
  /// The body ended with no sentinel: the connection closed.
  EndedWithoutSentinel,
  /// A non-streaming body was read and parsed in full.
  CompleteBody,
}

/// Accumulates chunks and preserves the ordering the harness contract requires.
#[derive(Default)]
pub struct Decoder {
  tools: std::collections::BTreeMap<u64, ToolBuilder>,
  finish_reason: Option<String>,
  input_tokens: Option<u64>,
  output_tokens: Option<u64>,
  /// Whether anything the user would call an answer has been emitted.
  emitted_output: bool,
}

#[derive(Default)]
struct ToolBuilder {
  id: Option<String>,
  name: String,
  arguments: String,
}

impl Decoder {
  /// Whether visible output has already reached the sink.
  pub fn emitted_output(&self) -> bool {
    self.emitted_output
  }

  /// Consume one streamed chunk, or a full one-shot completion.
  ///
  /// Accepts both shapes because the one-shot body is the same object with
  /// `choices[].message` instead of `choices[].delta`.
  /// The large error variant is deliberate: a failure must carry phase and
  /// partial-output state, and this is a cold path, matching
  /// [`pi_rs_core::ModelProvider::stream`].
  #[allow(clippy::result_large_err)]
  pub fn chunk(
    &mut self,
    chunk: &Value,
    sink: &mut dyn ProviderEventSink,
  ) -> Result<(), pi_rs_core::ModelFailure> {
    if let Some(usage) = chunk.get("usage").filter(|u| !u.is_null()) {
      self.input_tokens = usage
        .get("prompt_tokens")
        .or_else(|| usage.get("input_tokens"))
        .and_then(Value::as_u64)
        .or(self.input_tokens);
      self.output_tokens = usage
        .get("completion_tokens")
        .or_else(|| usage.get("output_tokens"))
        .and_then(Value::as_u64)
        .or(self.output_tokens);
    }

    let Some(choices) = chunk.get("choices").and_then(Value::as_array) else {
      // Usage-only chunks, and the empty-choices chunk some servers send last.
      return Ok(());
    };
    for choice in choices {
      let delta = choice
        .get("delta")
        .or_else(|| choice.get("message"))
        .cloned()
        .unwrap_or(Value::Null);
      if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
        self.finish_reason = Some(finish.to_string());
      }
      if let Some(reasoning) = reasoning_text(&delta) {
        if !reasoning.is_empty() {
          // Server-generated thinking, so the claim is Native and nothing else.
          sink.emit(&ProviderEvent::ReasoningDelta {
            text: reasoning.to_string(),
            provenance: ReasoningProvenance::Native,
          });
          self.emitted_output = true;
        }
      }
      if let Some(text) = content_text(&delta) {
        if !text.is_empty() {
          sink.emit(&ProviderEvent::TextDelta(text.to_string()));
          self.emitted_output = true;
        }
      }
      if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
          self.accumulate_tool(call)?;
        }
      }
    }
    Ok(())
  }

  /// Flush decoded tool calls and report usage.
  ///
  /// Tool calls are emitted only here. A fragment is not a tool call, and the
  /// contract says an adapter may not surface a partially decoded one.
  /// Cold path only: the typed failure carries phase and partial-output state, and
  /// boxing it would add indirection to every match without protecting a hot path.
  #[allow(clippy::result_large_err)]
  pub fn finish(
    mut self,
    end: StreamEnd,
    sink: &mut dyn ProviderEventSink,
  ) -> Result<CompletionUsage, pi_rs_core::ModelFailure> {
    let tools = std::mem::take(&mut self.tools);
    // A decoded tool call is output the provider produced, even though a user
    // would not call it an answer.
    let produced_a_call = !tools.is_empty();
    for (index, builder) in tools {
      let Some(id) = builder.id.filter(|id| !id.trim().is_empty()) else {
        return Err(decode_failure(format!(
          "tool call fragment {index} never carried an id"
        )));
      };
      let arguments = if builder.arguments.trim().is_empty() {
        Value::Object(serde_json::Map::new())
      } else {
        serde_json::from_str(&builder.arguments).map_err(|error| {
          decode_failure(format!(
            "tool call {id} arguments are not valid JSON: {error}"
          ))
          .with_detail(summarize(&builder.arguments))
        })?
      };
      sink.emit(&ProviderEvent::ToolCall(ToolCallBlock {
        id: ToolCallId::from_string(id),
        name: builder.name,
        arguments,
      }));
    }
    // A turn is complete only when the provider said so. A socket that simply
    // closed, or a stream that reached `[DONE]` without ever naming a finish
    // reason or producing output, is an interrupted turn; reporting success
    // there is how a harness silently truncates answers.
    let reported_completion = self.finish_reason.is_some();
    let certainty = match end {
      // The whole body parsed, so the server answered in full.
      StreamEnd::CompleteBody => CompletionCertainty::Certain,
      // `[DONE]` is the provider's own end-of-stream sentinel, so it settles
      // completion for a stream that produced something. An empty stream that
      // merely says `[DONE]` says nothing about completion.
      StreamEnd::DoneSentinel => {
        if reported_completion || self.emitted_output || produced_a_call {
          CompletionCertainty::Certain
        } else {
          return Err(decode_failure(
            "stream ended without a finish reason and without any output".to_string(),
          ));
        }
      }
      // EOF without the sentinel: the connection ended, the provider did not.
      StreamEnd::EndedWithoutSentinel => {
        if reported_completion {
          CompletionCertainty::Certain
        } else if self.emitted_output || produced_a_call {
          // Nothing was done wrong here, and retrying is not this layer's call: a
          // half-answer is already committed content. Report the boundary as
          // uncertain and let the runtime decide what unfinished means.
          CompletionCertainty::Unknown
        } else {
          // Nothing was committed, so a retry cannot duplicate anything. That is an
          // availability failure, which is what the transport layer is for.
          let mut failure = pi_rs_core::ModelFailure::new(
            pi_rs_core::ModelFailureKind::Transport,
            pi_rs_core::FailurePhase::Streaming,
            "stream ended before the provider reported completion",
          );
          failure.partial_output_emitted = false;
          return Err(failure);
        }
      }
    };
    Ok(CompletionUsage {
      input_tokens: self.input_tokens,
      output_tokens: self.output_tokens,
      finish_reason: self.finish_reason,
      certainty,
    })
  }

  /// Cold path only: the typed failure carries phase and partial-output state, and
  /// boxing it would add indirection to every match without protecting a hot path.
  #[allow(clippy::result_large_err)]
  fn accumulate_tool(&mut self, call: &Value) -> Result<(), pi_rs_core::ModelFailure> {
    let index = call
      .get("index")
      .and_then(Value::as_u64)
      // Servers that omit the index emit one tool call per chunk; treating a
      // missing index as 0 keeps single-call streams decoding.
      .unwrap_or(0);
    let builder = self.tools.entry(index).or_default();
    if let Some(id) = call.get("id").and_then(Value::as_str) {
      builder.id = Some(id.to_string());
    }
    if let Some(function) = call.get("function") {
      if let Some(name) = function.get("name").and_then(Value::as_str) {
        builder.name.push_str(name);
      }
      if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
        builder.arguments.push_str(arguments);
      }
    }
    Ok(())
  }
}

/// A failure while turning a valid response body into harness events.
///
/// The phase is `Normalizing`, not `Streaming`: the provider answered, and the
/// harness could not use the answer. Callers still have to set
/// `partial_output_emitted`, which only the surrounding loop knows.
fn decode_failure(message: String) -> pi_rs_core::ModelFailure {
  pi_rs_core::ModelFailure::new(
    pi_rs_core::ModelFailureKind::Protocol,
    FailurePhase::Normalizing,
    message,
  )
}

/// Where thinking text arrives, across the dialects seen in the wild.
fn reasoning_text(delta: &Value) -> Option<&str> {
  ["reasoning_content", "reasoning", "reasoning_text"]
    .iter()
    .find_map(|field| delta.get(*field).and_then(Value::as_str))
}

/// Visible assistant text, tolerating the string and part-array shapes.
///
/// Returns owned text: a parts array has to be joined anyway, and copying one
/// small delta is not a cost worth contorting the type for.
fn content_text(delta: &Value) -> Option<String> {
  match delta.get("content")? {
    Value::String(text) => Some(text.clone()),
    Value::Array(parts) => {
      // Parts are joined per chunk; the harness concatenates deltas anyway, so
      // a part split across a chunk boundary still reassembles.
      let mut joined = String::new();
      for part in parts {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
          joined.push_str(text);
        }
      }
      (!joined.is_empty()).then_some(joined)
    }
    _ => None,
  }
}

/// Classify an HTTP error status plus body.
pub fn http_failure(
  status: u16,
  body: &str,
  retry_after: Option<&str>,
  phase: FailurePhase,
) -> pi_rs_core::ModelFailure {
  let message = error_message(body);
  let kind = pi_rs_core::ModelFailure::classify_http(status, &message);
  pi_rs_core::ModelFailure::new(kind, phase, summarize(&message))
    .with_status(status)
    .with_retry_after_ms(retry_after_ms(retry_after, body).unwrap_or(0))
    .with_detail(truncate(body, MAX_ERROR_BODY_BYTES / 4))
}

/// Classify a transport-level failure. A read timeout is a distinct kind
/// because failover budgets and backoff differ between "connection never
/// happened" and "the answer stopped arriving".
pub fn transport_failure(text: &str, phase: FailurePhase) -> pi_rs_core::ModelFailure {
  let kind = if timed_out(text) {
    pi_rs_core::ModelFailureKind::Timeout
  } else {
    pi_rs_core::ModelFailureKind::Transport
  };
  pi_rs_core::ModelFailure::new(kind, phase, summarize(text))
}

/// Classify a failure while the body was streaming.
pub fn stream_failure(error: &io::Error, emitted_output: bool) -> pi_rs_core::ModelFailure {
  let text = error.to_string();
  let mut failure = transport_failure(&text, FailurePhase::Streaming);
  if matches!(
    error.kind(),
    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
  ) {
    failure.kind = pi_rs_core::ModelFailureKind::Timeout;
  }
  failure.partial_output_emitted = emitted_output;
  failure
}

/// The user-visible consequence of a cancel: not an availability failure.
pub fn cancelled(emitted_output: bool) -> pi_rs_core::ModelFailure {
  pi_rs_core::ModelFailure::new(
    pi_rs_core::ModelFailureKind::Cancelled,
    if emitted_output {
      FailurePhase::Streaming
    } else {
      FailurePhase::PreRequest
    },
    "cancelled by request",
  )
  .with_partial_output(emitted_output)
}

/// A cancel check between chunks.
pub fn check_cancel(
  cancel: &CancelToken,
  emitted_output: bool,
) -> Option<pi_rs_core::ModelFailure> {
  cancel.is_cancelled().then(|| cancelled(emitted_output))
}

/// Extract the provider's own error text from an error body.
fn error_message(body: &str) -> String {
  if let Ok(value) = serde_json::from_str::<Value>(body) {
    for path in [
      &["error", "message"][..],
      &["message"][..],
      &["error", "msg"][..],
      &["detail"][..],
    ] {
      let mut cursor = Some(&value);
      for key in path {
        cursor = cursor.and_then(|node| node.get(*key));
      }
      if let Some(Value::String(message)) = cursor {
        if !message.trim().is_empty() {
          return message.clone();
        }
      }
    }
  }
  let trimmed = body.trim();
  if trimmed.is_empty() {
    "<empty body>".to_string()
  } else {
    trimmed.to_string()
  }
}

/// Seconds or milliseconds, from header or body, normalized to milliseconds.
fn retry_after_ms(header: Option<&str>, body: &str) -> Option<u64> {
  if let Some(value) = header
    .and_then(|value| value.trim().parse::<f64>().ok())
    .filter(|value| *value >= 0.0)
  {
    return Some((value * 1_000.0) as u64);
  }
  let value = serde_json::from_str::<Value>(body).ok().and_then(|value| {
    value
      .pointer("/error/retry_after")
      .or_else(|| value.pointer("/retry_after"))
      .and_then(Value::as_u64)
  })?;
  Some(value * 1_000)
}

fn timed_out(text: &str) -> bool {
  let lower = text.to_lowercase();
  lower.contains("timed out") || lower.contains("timeout")
}

pub(crate) fn summarize(text: &str) -> String {
  let first_line = text
    .lines()
    .find(|line| !line.trim().is_empty())
    .unwrap_or("");
  truncate(first_line.trim(), 240)
}

fn truncate(text: &str, max_chars: usize) -> String {
  let count = text.chars().count();
  if count <= max_chars {
    return text.to_string();
  }
  let kept: String = text.chars().take(max_chars).collect();
  format!("{kept}\u{2026}({} chars truncated)", count - max_chars)
}

#[cfg(test)]
mod tests {
  use pi_rs_core::{Collector, ModelFailureKind};

  use super::*;

  fn chunk(data: Value) -> Value {
    json!({"choices": [{"index": 0, "delta": data, "finish_reason": null}]})
  }

  use serde_json::json;

  fn decode(chunks: &[Value]) -> (Collector, CompletionUsage) {
    let mut collector = Collector::default();
    let mut decoder = Decoder::default();
    for chunk in chunks {
      decoder.chunk(chunk, &mut collector).unwrap();
    }
    let usage = decoder
      .finish(StreamEnd::DoneSentinel, &mut collector)
      .unwrap();
    (collector, usage)
  }

  #[test]
  fn reasoning_and_text_keep_their_order_and_provenance() {
    let (collector, _) = decode(&[
      chunk(json!({"reasoning_content": "think "})),
      chunk(json!({"reasoning_content": "hard"})),
      chunk(json!({"content": "answer"})),
    ]);
    let events = collector.events();
    assert_eq!(events.len(), 3);
    assert!(matches!(
      &events[0],
      ProviderEvent::ReasoningDelta { text, provenance: ReasoningProvenance::Native }
        if text == "think "
    ));
    assert!(matches!(&events[1], ProviderEvent::ReasoningDelta { text, .. } if text == "hard"));
    assert!(matches!(&events[2], ProviderEvent::TextDelta(t) if t == "answer"));
  }

  #[test]
  fn dialect_variants_of_thinking_all_arrive_as_reasoning() {
    for field in ["reasoning_content", "reasoning", "reasoning_text"] {
      let (collector, _) = decode(&[chunk(json!({field: "thought"}))]);
      assert!(
        matches!(&collector.events()[0], ProviderEvent::ReasoningDelta { text, .. } if text == "thought"),
        "{field}"
      );
    }
  }

  #[test]
  fn content_parts_are_joined() {
    let (collector, _) = decode(&[chunk(json!({
      "content": [{"type": "text", "text": "one "}, {"type": "text", "text": "two"}]
    }))]);
    assert!(matches!(&collector.events()[0], ProviderEvent::TextDelta(t) if t == "one two"));
  }

  #[test]
  fn tool_call_fragments_become_one_decoded_call_after_the_stream_ends() {
    let mut collector = Collector::default();
    let mut decoder = Decoder::default();
    for fragment in [
      chunk(json!({
        "tool_calls": [{"index": 0, "id": "call_7", "function": {"name": "read", "arguments": "{\"pa"}}]
      })),
      chunk(json!({"tool_calls": [{"index": 0, "function": {"arguments": "th\": \"a.rs\"}"}}]})),
    ] {
      decoder.chunk(&fragment, &mut collector).unwrap();
    }
    assert!(
      collector.events().is_empty(),
      "a fragment is not a tool call"
    );
    decoder
      .finish(StreamEnd::DoneSentinel, &mut collector)
      .unwrap();
    let events = collector.events();
    assert_eq!(events.len(), 1);
    match &events[0] {
      ProviderEvent::ToolCall(call) => {
        assert_eq!(call.id.as_str(), "call_7");
        assert_eq!(call.name, "read");
        assert_eq!(call.arguments, json!({"path": "a.rs"}));
      }
      other => panic!("expected a tool call, got {other:?}"),
    }
  }

  #[test]
  fn several_tool_calls_in_one_response_keep_their_indices_apart() {
    let (collector, _) = decode(&[chunk(json!({
      "tool_calls": [
        {"index": 0, "id": "a", "function": {"name": "read", "arguments": "{}"}},
        {"index": 1, "id": "b", "function": {"name": "exec", "arguments": "{}"}},
      ]
    }))]);
    let names: Vec<&str> = collector
      .events()
      .iter()
      .map(|event| match event {
        ProviderEvent::ToolCall(call) => call.name.as_str(),
        _ => "",
      })
      .collect();
    assert_eq!(names, vec!["read", "exec"]);
  }

  #[test]
  fn missing_arguments_decode_to_an_empty_object() {
    let (collector, _) = decode(&[chunk(json!({
      "tool_calls": [{"index": 0, "id": "a", "function": {"name": "tick"}}]
    }))]);
    match &collector.events()[0] {
      ProviderEvent::ToolCall(call) => assert_eq!(call.arguments, json!({})),
      other => panic!("{other:?}"),
    }
  }

  #[test]
  fn malformed_tool_arguments_are_a_protocol_failure_not_a_tool_call() {
    let mut collector = Collector::default();
    let mut decoder = Decoder::default();
    decoder
      .chunk(
        &chunk(json!({"tool_calls": [{"index": 0, "id": "a", "function": {"name": "read", "arguments": "{\"path\": "}}]})),
        &mut collector,
      )
      .unwrap();
    let failure = decoder
      .finish(StreamEnd::DoneSentinel, &mut collector)
      .unwrap_err();
    assert_eq!(failure.kind, ModelFailureKind::Protocol);
    assert!(
      failure.message.contains("not valid JSON"),
      "{}",
      failure.message
    );
    assert!(
      collector.events().is_empty(),
      "nothing half-decoded escaped"
    );
  }

  #[test]
  fn a_tool_call_fragment_without_id_is_refused() {
    let mut collector = Collector::default();
    let mut decoder = Decoder::default();
    decoder
      .chunk(
        &chunk(
          json!({"tool_calls": [{"index": 3, "function": {"name": "read", "arguments": "{}"}}]}),
        ),
        &mut collector,
      )
      .unwrap();
    let failure = decoder
      .finish(StreamEnd::DoneSentinel, &mut collector)
      .unwrap_err();
    assert!(
      failure.message.contains("never carried an id"),
      "{}",
      failure.message
    );
  }

  #[test]
  fn usage_and_finish_reason_survive() {
    let mut collector = Collector::default();
    let mut decoder = Decoder::default();
    decoder
      .chunk(
        &json!({"choices": [{"index": 0, "delta": {"content": "hi"}, "finish_reason": "tool_calls"}]}),
        &mut collector,
      )
      .unwrap();
    decoder
      .chunk(
        &json!({"choices": [], "usage": {"prompt_tokens": 11, "completion_tokens": 4}}),
        &mut collector,
      )
      .unwrap();
    let usage = decoder
      .finish(StreamEnd::DoneSentinel, &mut collector)
      .unwrap();
    assert_eq!(usage.input_tokens, Some(11));
    assert_eq!(usage.output_tokens, Some(4));
    assert_eq!(usage.finish_reason.as_deref(), Some("tool_calls"));
  }

  #[test]
  fn emitted_output_is_reported_honestly() {
    let mut collector = Collector::default();
    let mut decoder = Decoder::default();
    assert!(!decoder.emitted_output());
    decoder
      .chunk(
        &chunk(json!({"reasoning_content": "think"})),
        &mut collector,
      )
      .unwrap();
    assert!(
      decoder.emitted_output(),
      "thinking is output the caller has seen"
    );
  }

  #[test]
  fn mid_stream_failure_marks_partial_output() {
    let mut collector = Collector::default();
    let mut decoder = Decoder::default();
    decoder
      .chunk(&chunk(json!({"content": "half"})), &mut collector)
      .unwrap();
    let failure = stream_failure(
      &io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed"),
      decoder.emitted_output(),
    );
    assert_eq!(failure.kind, ModelFailureKind::Transport);
    assert_eq!(failure.phase, FailurePhase::Streaming);
    assert!(failure.partial_output_emitted);
  }

  #[test]
  fn read_timeout_is_a_timeout_not_a_transport_error() {
    let failure = stream_failure(
      &io::Error::new(io::ErrorKind::TimedOut, "read timed out"),
      true,
    );
    assert_eq!(failure.kind, ModelFailureKind::Timeout);
    assert!(
      transport_failure("dns failure", FailurePhase::WaitingForResponse).kind
        == ModelFailureKind::Transport
    );
    assert_eq!(
      transport_failure("connection timed out", FailurePhase::WaitingForResponse).kind,
      ModelFailureKind::Timeout
    );
  }

  #[test]
  fn http_status_maps_and_body_message_is_surfaced() {
    let failure = http_failure(
      429,
      r#"{"error":{"message":"Too many requests","retry_after":2}}"#,
      None,
      FailurePhase::WaitingForResponse,
    );
    assert_eq!(failure.kind, ModelFailureKind::RateLimited);
    assert_eq!(failure.status, Some(429));
    assert_eq!(failure.retry_after_ms, Some(2_000));
    assert_eq!(failure.message, "Too many requests");

    assert_eq!(
      http_failure(
        401,
        r#"{"error":{"message":"bad key"}}"#,
        None,
        FailurePhase::WaitingForResponse
      )
      .kind,
      ModelFailureKind::Authentication
    );
    assert_eq!(
      http_failure(500, "internal", None, FailurePhase::WaitingForResponse).kind,
      ModelFailureKind::ProviderUnavailable
    );
  }

  #[test]
  fn context_overflow_beats_the_status_code() {
    let failure = http_failure(
      400,
      r#"{"error":{"message":"This model's maximum context length is 4096 tokens"}}"#,
      None,
      FailurePhase::WaitingForResponse,
    );
    assert_eq!(failure.kind, ModelFailureKind::ContextOverflow);
  }

  #[test]
  fn retry_after_header_wins_and_fractional_seconds_work() {
    let failure = http_failure(429, "{}", Some("1.5"), FailurePhase::WaitingForResponse);
    assert_eq!(failure.retry_after_ms, Some(1_500));
  }

  #[test]
  fn non_json_error_bodies_still_produce_a_readable_message() {
    let failure = http_failure(
      502,
      "<html><body>Bad Gateway\nmore text</body></html>",
      None,
      FailurePhase::WaitingForResponse,
    );
    assert_eq!(failure.message, "<html><body>Bad Gateway");
    assert!(failure.detail.is_some());
  }

  #[test]
  fn long_bodies_are_truncated_with_the_elision_visible() {
    let body = "x".repeat(100_000);
    let failure = http_failure(500, &body, None, FailurePhase::WaitingForResponse);
    // The one-line summary is bounded so it can be a status line, and the
    // elision is visible instead of looking like the server stopped talking.
    assert!(failure.message.chars().count() < 300, "{}", failure.message);
    assert!(failure.message.contains("chars truncated"));
    let detail = failure.detail.unwrap();
    assert!(detail.contains("chars truncated"), "{}", &detail[..80]);
  }

  #[test]
  fn cancel_is_not_an_availability_failure() {
    let cancel = CancelToken::new();
    assert!(check_cancel(&cancel, false).is_none());
    cancel.cancel();
    let failure = check_cancel(&cancel, true).unwrap();
    assert_eq!(failure.kind, ModelFailureKind::Cancelled);
    assert!(failure.partial_output_emitted);
    assert!(!failure.kind.is_retryable());
  }
}
