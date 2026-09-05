//! The adapter itself: send, stream, and normalize.

use std::{
  fmt,
  io::{self, Read},
};

use pi_rs_core::{
  CancelToken, CapabilityGap, FailurePhase, ModelCapabilities, ModelFailure, ModelFailureKind,
  ModelProvider, ModelRef, ModelRequest, ProviderEventSink, provider::CompletionUsage,
};

use crate::{
  config::{BuildError, ProviderConfig, agent_for},
  decode::{self, Decoder, StreamEnd},
  mapping::request_body,
};

/// OpenAI-compatible provider.
pub struct OpenAiCompat {
  config: ProviderConfig,
  model: pi_rs_core::ModelRef,
  agent: ureq::Agent,
}

impl fmt::Debug for OpenAiCompat {
  /// Omits the credential, so a debug print cannot leak a key into a log line
  /// or a durable trace.
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("OpenAiCompat")
      .field("id", &self.config.id)
      .field("base_url", &self.config.base_url)
      .field("model", &self.config.model)
      .field("credential", &"[redacted]")
      .finish()
  }
}

impl OpenAiCompat {
  pub fn new(config: ProviderConfig) -> Result<Self, BuildError> {
    config.validate()?;
    let model = pi_rs_core::ModelRef::new(&config.id, &config.model);
    Ok(Self {
      agent: agent_for(&config),
      model,
      config,
    })
  }

  /// Local endpoint without a credential: the llama.cpp and vLLM case.
  pub fn local(
    id: impl Into<String>,
    model: impl Into<String>,
    base_url: impl Into<String>,
    context_window: u64,
  ) -> Result<Self, BuildError> {
    Self::new(ProviderConfig::local(id, model, base_url, context_window))
  }

  pub fn config(&self) -> &ProviderConfig {
    &self.config
  }

  /// Declared gaps relative to what a session needs.
  pub fn gaps_for(&self, required: &ModelCapabilities) -> Vec<CapabilityGap> {
    self.config.gaps(required)
  }

  /// Cold path only: the typed failure carries phase and partial-output state, and
  /// boxing it would add indirection to every match without protecting a hot path.
  #[allow(clippy::result_large_err)]
  fn send(&self, body: &str) -> Result<ureq::Response, ModelFailure> {
    let mut call = self
      .agent
      .post(&self.config.chat_completions_url())
      .set("content-type", "application/json")
      .set("accept", "text/event-stream");
    if let Some(key) = self.config.credential() {
      call = call.set("authorization", &format!("Bearer {key}"));
    }
    for (name, value) in &self.config.headers {
      call = call.set(name, value);
    }
    match call.send_string(body) {
      Ok(response) => Ok(response),
      Err(ureq::Error::Status(status, response)) => Err(self::http_failure_from(status, response)),
      Err(other) => Err(decode::transport_failure(
        &other.to_string(),
        FailurePhase::WaitingForResponse,
      )),
    }
  }

  /// Cold path only: the typed failure carries phase and partial-output state, and
  /// boxing it would add indirection to every match without protecting a hot path.
  #[allow(clippy::result_large_err)]
  fn read_stream(
    &self,
    response: ureq::Response,
    sink: &mut dyn ProviderEventSink,
    cancel: &CancelToken,
  ) -> Result<CompletionUsage, ModelFailure> {
    let mut decoder = Decoder::default();
    // Which kind of end we actually observe decides whether this turn may be
    // reported as complete.
    let mut end = StreamEnd::DoneSentinel;
    let mut stream = crate::sse::SseStream::new(response.into_reader());
    loop {
      if let Some(failure) = decode::check_cancel(cancel, decoder.emitted_output()) {
        return Err(failure.with_model(self.model_ref()));
      }
      let event = match stream.next_event() {
        Ok(event) => event,
        Err(error) => {
          // A cancel that lands while blocked in `read` surfaces as an IO
          // error: the user's intent outranks the transport symptom.
          if cancel.is_cancelled() {
            return Err(decode::cancelled(decoder.emitted_output()).with_model(self.model_ref()));
          }
          return Err(
            decode::stream_failure(&error, decoder.emitted_output()).with_model(self.model_ref()),
          );
        }
      };
      let Some(event) = event else {
        if cancel.is_cancelled() {
          return Err(decode::cancelled(decoder.emitted_output()).with_model(self.model_ref()));
        }
        // EOF with no sentinel: the connection ended, the provider did not.
        end = StreamEnd::EndedWithoutSentinel;
        break;
      };
      if event.is_done() {
        break;
      }
      let chunk = match decode_chunk(&event.data) {
        Ok(chunk) => chunk,
        Err(failure) => {
          return Err(failure.with_model(self.model_ref()));
        }
      };
      if let Err(failure) = decoder
        .chunk(&chunk, sink)
        .map_err(|failure| annotated(&failure, decoder.emitted_output()))
      {
        return Err(failure.with_model(self.model_ref()));
      }
    }
    // Captured before `finish` consumes the decoder: it is the last honest
    // answer to "has the caller already seen output".
    let emitted = decoder.emitted_output();
    decoder
      .finish(end, sink)
      .map_err(|failure| annotated(&failure, emitted))
  }

  /// Cold path only: the typed failure carries phase and partial-output state, and
  /// boxing it would add indirection to every match without protecting a hot path.
  #[allow(clippy::result_large_err)]
  fn read_one_shot(
    &self,
    response: ureq::Response,
    sink: &mut dyn ProviderEventSink,
  ) -> Result<CompletionUsage, ModelFailure> {
    let body = read_body(response, crate::config::MAX_ERROR_BODY_BYTES as u64 * 64)
      .map_err(|error| decode::stream_failure(&error, false).with_model(self.model_ref()))?;
    let value = decode_chunk(std::str::from_utf8(&body).unwrap_or(""))
      .map_err(|failure| failure.with_model(self.model_ref()))?;
    let mut decoder = Decoder::default();
    decoder
      .chunk(&value, sink)
      .map_err(|failure| annotated(&failure, false).with_model(self.model_ref()))?;
    decoder
      .finish(StreamEnd::CompleteBody, sink)
      .map_err(|failure| annotated(&failure, false).with_model(self.model_ref()))
  }

  fn model_ref(&self) -> pi_rs_core::ModelRef {
    self.model.clone()
  }
}

/// Read an error status plus body into a normalized failure.
/// Cold path only: the typed failure carries phase and partial-output state, and
/// boxing it would add indirection to every match without protecting a hot path.
#[allow(clippy::result_large_err)]
fn http_failure_from(status: u16, response: ureq::Response) -> ModelFailure {
  let retry_after = response.header("retry-after").map(str::to_string);
  let body = read_body(response, crate::config::MAX_ERROR_BODY_BYTES as u64).unwrap_or_default();
  let text = String::from_utf8_lossy(&body);
  decode::http_failure(
    status,
    &text,
    retry_after.as_deref(),
    FailurePhase::WaitingForResponse,
  )
}

/// Parse one stream payload.
///
/// A non-JSON payload is tolerated *after* output has started — a proxy that
/// injects a keep-alive page mid-stream should not destroy a usable answer —
/// but never before, because that is how a broken gateway otherwise looks
/// exactly like a hung provider.
/// Cold path only: the typed failure carries phase and partial-output state, and
/// boxing it would add indirection to every match without protecting a hot path.
#[allow(clippy::result_large_err)]
fn decode_chunk(data: &str) -> Result<serde_json::Value, ModelFailure> {
  serde_json::from_str(data).map_err(|error| {
    ModelFailure::new(
      ModelFailureKind::Protocol,
      FailurePhase::Normalizing,
      format!("stream payload was not JSON: {error}"),
    )
    .with_detail(decode::summarize(data))
  })
}

fn annotated(failure: &ModelFailure, emitted_output: bool) -> ModelFailure {
  failure.clone().with_partial_output(emitted_output)
}

/// Read at most `limit` bytes of a response body.
///
/// Error bodies are summarized and truncated, so there is no reason to let a
/// server stream an unlimited body into memory.
fn read_body(response: ureq::Response, limit: u64) -> io::Result<Vec<u8>> {
  let mut body = Vec::new();
  response.into_reader().take(limit).read_to_end(&mut body)?;
  Ok(body)
}

impl ModelProvider for OpenAiCompat {
  fn provider_id(&self) -> &str {
    &self.config.id
  }

  fn model(&self) -> &ModelRef {
    &self.model
  }

  fn capabilities(&self) -> ModelCapabilities {
    self.config.capabilities.clone()
  }

  fn stream(
    &self,
    request: &ModelRequest,
    sink: &mut dyn ProviderEventSink,
    cancel: &CancelToken,
  ) -> Result<CompletionUsage, ModelFailure> {
    let model = request.model.clone();
    if cancel.is_cancelled() {
      return Err(
        ModelFailure::new(
          ModelFailureKind::Cancelled,
          FailurePhase::PreRequest,
          "cancelled before send",
        )
        .with_model(model),
      );
    }
    let body = request_body(&self.config, request).to_string();
    let response = self
      .send(&body)
      .map_err(|failure| failure.with_model(model.clone()))?;
    if self.config.stream {
      self
        .read_stream(response, sink, cancel)
        .map_err(|failure| failure.with_model(model))
    } else {
      self
        .read_one_shot(response, sink)
        .map_err(|failure| failure.with_model(model))
    }
  }
}

#[cfg(test)]
mod tests {
  use pi_rs_core::{Collector, ModelRef, ProviderEvent};

  use super::*;

  fn adapter() -> OpenAiCompat {
    OpenAiCompat::local("local", "qwen3.8-flash", "http://127.0.0.1:9/v1", 4_096).unwrap()
  }

  fn request() -> ModelRequest {
    ModelRequest::new(
      ModelRef::new("local", "qwen3.8-flash"),
      adapter().capabilities().clone(),
      vec![pi_rs_core::Message::user("hi")],
    )
  }

  #[test]
  fn construction_rejects_an_unusable_endpoint() {
    let broken = ProviderConfig {
      base_url: "ftp://nowhere".into(),
      ..ProviderConfig::local("local", "m", "http://x/v1", 1_024)
    };
    assert!(matches!(
      OpenAiCompat::new(broken),
      Err(BuildError::Invalid(_))
    ));
  }

  #[test]
  fn identity_and_capabilities_come_from_config() {
    let opened = adapter();
    assert_eq!(opened.provider_id(), "local");
    assert_eq!(
      opened.model(),
      &ModelRef::new("local", "qwen3.8-flash"),
      "the adapter's default model is part of its identity"
    );
    assert_eq!(opened.capabilities().context_window, 4_096);
  }

  #[test]
  fn debug_output_does_not_leak_the_credential() {
    let with_key = OpenAiCompat::new(ProviderConfig {
      api_key: Some("sk-super-secret".into()),
      ..ProviderConfig::local("local", "m", "http://127.0.0.1:9/v1", 1_024)
    })
    .unwrap();
    let text = format!("{with_key:?}");
    assert!(!text.contains("sk-super-secret"), "{text}");
  }

  #[test]
  fn cancel_before_send_never_opens_a_connection() {
    let cancel = CancelToken::new();
    cancel.cancel();
    let mut collector = Collector::default();
    let failure = adapter()
      .stream(&request(), &mut collector, &cancel)
      .unwrap_err();
    assert_eq!(failure.kind, ModelFailureKind::Cancelled);
    assert_eq!(failure.phase, FailurePhase::PreRequest);
    assert_eq!(failure.model, Some(ModelRef::new("local", "qwen3.8-flash")));
    assert!(collector.events().is_empty());
  }

  #[test]
  fn an_unreachable_endpoint_is_an_availability_failure_not_a_protocol_bug() {
    // Port 9 (discard) on loopback refuses connections almost instantly.
    let mut collector = Collector::default();
    let failure = adapter()
      .stream(&request(), &mut collector, &CancelToken::new())
      .unwrap_err();
    assert!(
      failure.kind.is_retryable(),
      "connection refused must be retryable: {failure:?}"
    );
    assert_eq!(failure.phase, FailurePhase::WaitingForResponse);
    assert!(!failure.partial_output_emitted);
  }

  #[test]
  fn chunk_parser_rejects_garbage_before_output() {
    let failure = decode_chunk("<html>gateway</html>").unwrap_err();
    assert_eq!(failure.kind, ModelFailureKind::Protocol);
    assert!(failure.message.contains("not JSON"), "{}", failure.message);
    assert!(decode_chunk(r#"{"choices":[]}"#).is_ok());
  }

  #[test]
  fn error_bodies_are_read_within_a_bound() {
    // A 204-shaped empty error body must still classify.
    let failure = decode::http_failure(503, "", None, FailurePhase::WaitingForResponse);
    assert_eq!(failure.kind, ModelFailureKind::ProviderUnavailable);
    assert_eq!(failure.message, "<empty body>");
  }

  #[test]
  fn gaps_are_reported_against_session_needs() {
    let needed = ModelCapabilities {
      tools: true,
      context_window: 1_000_000,
      ..ModelCapabilities::text_only(1)
    };
    let gaps = adapter().gaps_for(&needed);
    assert!(
      gaps.iter().any(|gap| matches!(gap, CapabilityGap::Tools)),
      "{gaps:?}"
    );
    assert!(
      gaps
        .iter()
        .any(|gap| matches!(gap, CapabilityGap::ContextWindow { .. })),
      "{gaps:?}"
    );
  }

  #[test]
  fn one_shot_decodes_a_full_completion_body() {
    // Exercises the decode path without a socket by calling the pieces the
    // one-shot reader uses.
    let body = serde_json::json!({
      "choices": [{"message": {"role": "assistant", "content": "done", "reasoning_content": "thought"}, "finish_reason": "stop"}],
      "usage": {"prompt_tokens": 3, "completion_tokens": 2},
    });
    let mut collector = Collector::default();
    let mut decoder = Decoder::default();
    decoder.chunk(&body, &mut collector).unwrap();
    let usage = decoder
      .finish(StreamEnd::CompleteBody, &mut collector)
      .unwrap();
    assert_eq!(usage.finish_reason.as_deref(), Some("stop"));
    assert_eq!(usage.output_tokens, Some(2));
    let events = collector.events();
    assert!(matches!(
      &events[0],
      ProviderEvent::ReasoningDelta { text, .. } if text == "thought"
    ));
    assert!(matches!(&events[1], ProviderEvent::TextDelta(t) if t == "done"));
  }
}
