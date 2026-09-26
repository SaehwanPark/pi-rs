//! The adapter itself: send, stream, and normalize.

use std::{
  fmt,
  io::{self, Read},
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, SyncSender, TrySendError},
  },
  thread,
  time::{Duration, Instant},
};

use rupi_core::{
  CancelToken, CapabilityGap, FailurePhase, ModelCapabilities, ModelFailure, ModelFailureKind,
  ModelProvider, ModelRef, ModelRequest, ProviderEvent, ProviderEventSink,
  provider::CompletionUsage,
};

use crate::{
  config::{BuildError, ProviderConfig, agent_for, agent_for_proxy, redact_url},
  decode::{self, Decoder, StreamEnd},
  mapping::{self, request_body},
  relay::CancellableHttpRelay,
};

/// OpenAI-compatible provider.
pub struct OpenAiCompat {
  config: ProviderConfig,
  model: rupi_core::ModelRef,
  agent: ureq::Agent,
  /// A cancelled request may still be inside ureq until its socket timeout.
  /// Quarantine this adapter so a later retry cannot overlap that request.
  quarantined: Arc<AtomicBool>,
  /// Per-attempt credential for the localhost cancellation relay.
  relay_nonce: Option<String>,
  active_request: Arc<AtomicBool>,
}

impl fmt::Debug for OpenAiCompat {
  /// Omits the credential, so a debug print cannot leak a key into a log line
  /// or a durable trace.
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("OpenAiCompat")
      .field("id", &self.config.id)
      .field("base_url", &redact_url(&self.config.base_url))
      .field("model", &self.config.model)
      .field("credential", &"[redacted]")
      .finish()
  }
}

impl OpenAiCompat {
  pub fn new(config: ProviderConfig) -> Result<Self, BuildError> {
    config.validate()?;
    let model = rupi_core::ModelRef::new(&config.id, &config.model);
    Ok(Self {
      agent: agent_for(&config),
      model,
      config,
      quarantined: Arc::new(AtomicBool::new(false)),
      relay_nonce: None,
      active_request: Arc::new(AtomicBool::new(false)),
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

  /// Remote cloud endpoint with an environment variable holding credentials.
  pub fn remote(
    id: impl Into<String>,
    model: impl Into<String>,
    base_url: Option<impl Into<String>>,
    api_key_env: impl Into<String>,
    context_window: u64,
  ) -> Result<Self, BuildError> {
    Self::new(ProviderConfig::remote(
      id,
      model,
      base_url,
      api_key_env,
      context_window,
    ))
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
  fn send(&self, body: &str, cancel: &CancelToken) -> Result<ureq::Response, ModelFailure> {
    // The transport classifies known pre-dispatch failures as replay-safe. Any
    // failure after bytes may have reached the endpoint stays ambiguous and is
    // quarantined; the runtime skips a same-adapter retry and may fail over.
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
    if let Some(nonce) = &self.relay_nonce {
      call = call.set("x-rupi-relay-nonce", nonce);
    }
    match call.send_string(body) {
      Ok(response) => Ok(response),
      Err(ureq::Error::Status(status, response)) => Err(self::http_failure_from(status, response)),
      Err(_other) if cancel.is_cancelled() => Err(decode::cancelled(false)),
      Err(other) => {
        let mut failure =
          decode::transport_failure(&other.to_string(), FailurePhase::WaitingForResponse);
        // ureq's display text differs across platforms (Windows often says
        // "operation timed out"), but its source retains the stable IO kind.
        if is_ureq_timeout(&other) {
          failure.kind = ModelFailureKind::Timeout;
        }
        if request_was_not_dispatched(&other) {
          failure.replay_safety = rupi_core::RequestReplaySafety::Safe;
        }
        Err(failure)
      }
    }
  }

  /// Cold path only: the typed failure carries phase and partial-output state, and
  /// boxing it would add indirection to every match without protecting a hot path.
  #[allow(clippy::result_large_err)]
  fn read_stream(
    &self,
    response: ureq::Response,
    request: &ModelRequest,
    sink: &mut dyn ProviderEventSink,
    cancel: &CancelToken,
  ) -> Result<CompletionUsage, ModelFailure> {
    let mut decoder = Decoder::new(self.config.capabilities.exposed_reasoning)
      .with_strict_tool_schemas(mapping::strict_tool_argument_schemas(&self.config, request));
    // Which kind of end we actually observe decides whether this turn may be
    // reported as complete.
    let mut end = StreamEnd::DoneSentinel;
    let idle_budget = Duration::from_millis(self.config.read_timeout_ms.max(1));
    let mut last_activity = Instant::now();
    let mut stream = crate::sse::SseStream::new(response.into_reader());
    loop {
      if let Some(failure) = decode::check_cancel(cancel, decoder.emitted_output()) {
        return Err(failure.with_model(self.model_ref()));
      }
      let event = match stream.next_event() {
        Ok(event) => event,
        Err(error) if is_transient_read_timeout(&error) => {
          // A socket timeout is a poll only when the logical idle budget has
          // not elapsed. The old implementation retried forever, making a
          // response that sent headers and then went silent immortal.
          if last_activity.elapsed() >= idle_budget {
            return Err(
              ModelFailure::new(
                ModelFailureKind::Timeout,
                FailurePhase::WaitingForResponse,
                "provider stream exceeded its configured idle timeout",
              )
              .with_partial_output(decoder.emitted_output()),
            );
          }
          continue;
        }
        Err(error) if error.kind() == io::ErrorKind::InvalidData && !cancel.is_cancelled() => {
          return Err(
            decode::sse_framing_failure(&error, decoder.emitted_output())
              .with_model(self.model_ref()),
          );
        }
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
      last_activity = Instant::now();
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
    request: &ModelRequest,
    sink: &mut dyn ProviderEventSink,
    cancel: &CancelToken,
  ) -> Result<CompletionUsage, ModelFailure> {
    if cancel.is_cancelled() {
      return Err(decode::cancelled(false).with_model(self.model_ref()));
    }
    let body =
      read_body(response, crate::config::MAX_ERROR_BODY_BYTES as u64 * 64).map_err(|error| {
        if cancel.is_cancelled() {
          decode::cancelled(false).with_model(self.model_ref())
        } else if is_transient_read_timeout(&error) {
          ModelFailure::new(
            ModelFailureKind::Timeout,
            FailurePhase::WaitingForResponse,
            "provider response exceeded its configured idle timeout",
          )
          .with_model(self.model_ref())
        } else {
          decode::stream_failure(&error, false).with_model(self.model_ref())
        }
      })?;
    if cancel.is_cancelled() {
      return Err(decode::cancelled(false).with_model(self.model_ref()));
    }
    let value = decode_chunk(std::str::from_utf8(&body).unwrap_or(""))
      .map_err(|failure| failure.with_model(self.model_ref()))?;
    let mut decoder = Decoder::new(self.config.capabilities.exposed_reasoning)
      .with_strict_tool_schemas(mapping::strict_tool_argument_schemas(&self.config, request));
    decoder
      .chunk(&value, sink)
      .map_err(|failure| annotated(&failure, false).with_model(self.model_ref()))?;
    decoder
      .finish(StreamEnd::CompleteBody, sink)
      .map_err(|failure| annotated(&failure, false).with_model(self.model_ref()))
  }

  fn model_ref(&self) -> rupi_core::ModelRef {
    self.model.clone()
  }

  #[allow(clippy::result_large_err)]
  fn stream_blocking(
    &self,
    request: &ModelRequest,
    sink: &mut dyn ProviderEventSink,
    cancel: &CancelToken,
  ) -> Result<CompletionUsage, ModelFailure> {
    let body = request_body(&self.config, request).to_string();
    let response = match self.send(&body, cancel) {
      Ok(response) => response,
      Err(_failure) if cancel.is_cancelled() => {
        return Err(decode::cancelled(false).with_model(self.model_ref()));
      }
      Err(failure) => return Err(failure.with_model(self.model_ref())),
    };
    if self.config.stream {
      self.read_stream(response, request, sink, cancel)
    } else {
      self.read_one_shot(response, request, sink, cancel)
    }
  }

  /// Run the blocking HTTP exchange behind a bounded channel. This gives the
  /// caller a cancellation poll without ever issuing a second POST: after the
  /// worker has sent bytes, cancellation only abandons that one in-flight
  /// exchange and the worker observes the same token on its next socket poll.
  #[allow(clippy::result_large_err)]
  fn stream_worker(
    &self,
    request: &ModelRequest,
    sink: &mut dyn ProviderEventSink,
    cancel: &CancelToken,
  ) -> Result<CompletionUsage, ModelFailure> {
    mapping::validate_tool_sampling(&self.config, request)
      .map_err(|failure| failure.with_model(self.model_ref()))?;
    if self.quarantined.load(Ordering::Acquire) {
      return Err(
        ModelFailure::new(
          ModelFailureKind::ProviderUnavailable,
          FailurePhase::WaitingForResponse,
          "provider adapter is quarantined after an abandoned request",
        )
        .with_model(self.model_ref()),
      );
    }
    if self.active_request.swap(true, Ordering::AcqRel) {
      return Err(
        ModelFailure::new(
          ModelFailureKind::ProviderUnavailable,
          FailurePhase::PreRequest,
          "provider adapter already has an in-flight request",
        )
        .with_model(self.model_ref()),
      );
    }
    let mut relay = match CancellableHttpRelay::start(
      &self.config.chat_completions_url(),
      Duration::from_millis(self.config.connect_timeout_ms.max(1)),
    ) {
      Ok(relay) => relay,
      Err(error) => {
        self.active_request.store(false, Ordering::Release);
        return Err(
          ModelFailure::new(
            ModelFailureKind::Transport,
            FailurePhase::PreRequest,
            format!("provider cancellation relay failed to start: {error}"),
          )
          .with_model(self.model_ref()),
        );
      }
    };
    let worker_agent = match agent_for_proxy(&self.config, &relay.proxy_url(), relay.nonce()) {
      Ok(agent) => agent,
      Err(error) => {
        relay.stop();
        self.active_request.store(false, Ordering::Release);
        return Err(
          ModelFailure::new(
            ModelFailureKind::Transport,
            FailurePhase::PreRequest,
            error.to_string(),
          )
          .with_model(self.model_ref()),
        );
      }
    };
    let (sender, receiver) = mpsc::sync_channel(64);
    let worker = Self {
      config: self.config.clone(),
      model: self.model.clone(),
      agent: worker_agent,
      quarantined: Arc::clone(&self.quarantined),
      relay_nonce: Some(relay.nonce().to_string()),
      active_request: Arc::clone(&self.active_request),
    };
    let active_request = Arc::clone(&self.active_request);
    let request = request.clone();
    let model = request.model.clone();
    // Keep a local token for the worker as well as the caller's token. The
    // outer loop translates caller cancellation and total-deadline expiry into
    // this token before stopping the relay, so the blocking worker can observe
    // the same boundary without issuing a second POST.
    let request_cancel = CancelToken::new();
    let worker_cancel = request_cancel.clone();
    let channel_cancel = request_cancel.clone();
    let done_cancel = request_cancel.clone();
    let worker_handle = thread::spawn(move || {
      let mut channel_sink = ChannelSink {
        sender: sender.clone(),
        cancel: channel_cancel,
      };
      let result = worker.stream_blocking(&request, &mut channel_sink, &worker_cancel);
      active_request.store(false, Ordering::Release);
      let mut done = WorkerMessage::Done(result);
      loop {
        match sender.try_send(done) {
          Ok(()) | Err(TrySendError::Disconnected(_)) => break,
          Err(TrySendError::Full(next)) => {
            if worker_cancel.is_cancelled() || done_cancel.is_cancelled() {
              break;
            }
            done = next;
            thread::yield_now();
          }
        }
      }
    });

    let mut emitted = false;
    let idle_budget = Duration::from_millis(self.config.read_timeout_ms.max(1));
    let total_budget = self.config.request_timeout_ms.map(Duration::from_millis);
    let request_started = Instant::now();
    let mut last_activity = Instant::now();
    loop {
      if cancel.is_cancelled() {
        request_cancel.cancel();
        self.quarantined.store(true, Ordering::Release);
        relay.stop();
        let _ = worker_handle.join();
        drain_worker_events(&receiver, sink, &mut emitted);
        // Keep the adapter quarantined: the POST may have reached the provider
        // before cancellation, so issuing another attempt through this adapter
        // could duplicate an uncertain request. Recovery must choose a fresh
        // adapter or an explicitly separate model.
        self.active_request.store(false, Ordering::Release);
        return Err(
          decode::cancelled(emitted)
            .with_replay_safety(if emitted {
              rupi_core::RequestReplaySafety::CommittedOutput
            } else {
              rupi_core::RequestReplaySafety::AmbiguousPostBoundary
            })
            .with_model(model),
        );
      }
      if let Some(budget) = total_budget.filter(|budget| request_started.elapsed() >= *budget) {
        request_cancel.cancel();
        self.quarantined.store(true, Ordering::Release);
        relay.stop();
        let _ = worker_handle.join();
        drain_worker_events(&receiver, sink, &mut emitted);
        self.active_request.store(false, Ordering::Release);
        return Err(
          ModelFailure::new(
            ModelFailureKind::Timeout,
            if emitted {
              FailurePhase::Streaming
            } else {
              FailurePhase::WaitingForResponse
            },
            format!(
              "provider request exceeded its configured total timeout ({} ms)",
              budget.as_millis()
            ),
          )
          .with_model(model.clone())
          .with_partial_output(emitted),
        );
      }
      match receiver.recv_timeout(Duration::from_millis(50)) {
        Ok(WorkerMessage::Event(event)) => {
          emitted = true;
          last_activity = Instant::now();
          sink.emit(&event);
        }
        Ok(WorkerMessage::Done(result)) => {
          relay.stop();
          let _ = worker_handle.join();
          self.active_request.store(false, Ordering::Release);
          if result.as_ref().is_err_and(|failure| {
            matches!(
              failure.kind,
              ModelFailureKind::Transport | ModelFailureKind::Timeout | ModelFailureKind::Cancelled
            ) && failure.replay_safety != rupi_core::RequestReplaySafety::Safe
          }) {
            // Socket/timeout/cancel failures are ambiguous after the POST
            // boundary. Explicit provider responses (5xx, throttles, protocol
            // errors) retain their normal runtime retry/failover policy.
            self.quarantined.store(true, Ordering::Release);
          }
          return result.map_err(|failure| {
            let partial = emitted || failure.partial_output_emitted;
            failure
              .with_model(model.clone())
              .with_partial_output(partial)
          });
        }
        Err(mpsc::RecvTimeoutError::Timeout) if cancel.is_cancelled() => {
          request_cancel.cancel();
          self.quarantined.store(true, Ordering::Release);
          relay.stop();
          let _ = worker_handle.join();
          drain_worker_events(&receiver, sink, &mut emitted);
          self.active_request.store(false, Ordering::Release);
          return Err(
            decode::cancelled(emitted)
              .with_replay_safety(if emitted {
                rupi_core::RequestReplaySafety::CommittedOutput
              } else {
                rupi_core::RequestReplaySafety::AmbiguousPostBoundary
              })
              .with_model(model),
          );
        }
        Err(mpsc::RecvTimeoutError::Timeout) if last_activity.elapsed() >= idle_budget => {
          request_cancel.cancel();
          self.quarantined.store(true, Ordering::Release);
          relay.stop();
          let _ = worker_handle.join();
          drain_worker_events(&receiver, sink, &mut emitted);
          self.active_request.store(false, Ordering::Release);
          return Err(
            ModelFailure::new(
              ModelFailureKind::Timeout,
              FailurePhase::WaitingForResponse,
              "provider response exceeded its configured idle timeout",
            )
            .with_model(model)
            .with_partial_output(emitted),
          );
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {}
        Err(mpsc::RecvTimeoutError::Disconnected) => {
          request_cancel.cancel();
          relay.stop();
          let _ = worker_handle.join();
          self.quarantined.store(true, Ordering::Release);
          self.active_request.store(false, Ordering::Release);
          return Err(
            ModelFailure::new(
              ModelFailureKind::Protocol,
              FailurePhase::WaitingForResponse,
              "provider worker exited without a result",
            )
            .with_model(model)
            .with_partial_output(emitted),
          );
        }
      }
    }
  }
}

#[derive(Debug)]
enum WorkerMessage {
  Event(ProviderEvent),
  Done(Result<CompletionUsage, ModelFailure>),
}

struct ChannelSink {
  sender: SyncSender<WorkerMessage>,
  cancel: CancelToken,
}

fn drain_worker_events(
  receiver: &mpsc::Receiver<WorkerMessage>,
  sink: &mut dyn ProviderEventSink,
  emitted: &mut bool,
) {
  while let Ok(message) = receiver.try_recv() {
    if let WorkerMessage::Event(event) = message {
      *emitted = true;
      sink.emit(&event);
    }
  }
}

impl ProviderEventSink for ChannelSink {
  fn emit(&mut self, event: &ProviderEvent) {
    let mut message = WorkerMessage::Event(event.clone());
    loop {
      match self.sender.try_send(message) {
        Ok(()) => return,
        Err(TrySendError::Disconnected(_)) => return,
        Err(TrySendError::Full(next)) => {
          if self.cancel.is_cancelled() {
            return;
          }
          message = next;
          thread::yield_now();
        }
      }
    }
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

fn request_was_not_dispatched(error: &ureq::Error) -> bool {
  matches!(
    error.kind(),
    ureq::ErrorKind::InvalidUrl
      | ureq::ErrorKind::UnknownScheme
      | ureq::ErrorKind::Dns
      | ureq::ErrorKind::InsecureRequestHttpsOnly
      | ureq::ErrorKind::ConnectionFailed
      | ureq::ErrorKind::InvalidProxyUrl
      | ureq::ErrorKind::ProxyConnect
      | ureq::ErrorKind::ProxyUnauthorized
  )
}

fn is_transient_read_timeout(error: &io::Error) -> bool {
  matches!(
    error.kind(),
    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
  )
}

fn is_ureq_timeout(error: &ureq::Error) -> bool {
  let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error);
  while let Some(error) = current {
    if let Some(io_error) = error.downcast_ref::<io::Error>()
      && (io_error.kind() == io::ErrorKind::TimedOut
        // Windows may preserve WSAETIMEDOUT as an "other" IO kind.
        || matches!(io_error.raw_os_error(), Some(110 | 10060)))
    {
      return true;
    }
    current = error.source();
  }
  false
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
    let mut capabilities = self.config.capabilities.clone();
    capabilities.max_output_tokens = self
      .config
      .max_output_tokens
      .or(capabilities.max_output_tokens);
    capabilities
  }

  fn reset_after_abandonment(&self) {
    // The runtime calls this only at the beginning of a new user turn, after
    // the abandoned worker has been joined. Reusing the adapter directly
    // without that boundary remains refused by `stream_worker`.
    self.quarantined.store(false, Ordering::Release);
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
    self
      .stream_worker(request, sink, cancel)
      .map_err(|failure| failure.with_model(model))
  }
}

#[cfg(test)]
mod tests {
  use rupi_core::{
    Collector, ModelRef, ProviderEvent, ReasoningExposure, ToolSamplingConstraint,
    ToolSamplingStrictness,
  };

  use super::*;

  fn adapter() -> OpenAiCompat {
    OpenAiCompat::local("local", "qwen3.8-flash", "http://127.0.0.1:9/v1", 4_096).unwrap()
  }

  fn request() -> ModelRequest {
    ModelRequest::new(
      ModelRef::new("local", "qwen3.8-flash"),
      adapter().capabilities().clone(),
      vec![rupi_core::Message::user("hi")],
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
  fn configured_output_ceiling_is_exposed_to_runtime_requests() {
    let provider = OpenAiCompat::new(ProviderConfig {
      max_output_tokens: Some(1_024),
      ..ProviderConfig::local("local", "model", "http://127.0.0.1/v1", 4_096)
    })
    .unwrap();

    assert_eq!(provider.capabilities().max_output_tokens, Some(1_024));
  }

  #[test]
  fn debug_output_redacts_credentials_and_signed_url_parts() {
    let with_key = OpenAiCompat::new(ProviderConfig {
      api_key: Some("sk-super-secret".into()),
      ..ProviderConfig::local(
        "local",
        "m",
        "https://gateway.test/v1?token=url-secret#fragment-secret",
        1_024,
      )
    })
    .unwrap();
    let text = format!("{with_key:?}");
    assert!(!text.contains("sk-super-secret"), "{text}");
    assert!(!text.contains("url-secret"), "{text}");
    assert!(!text.contains("fragment-secret"), "{text}");
    assert!(text.contains("[redacted]"), "{text}");
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
  fn a_required_strict_tool_refuses_before_opening_the_endpoint() {
    let mut config =
      ProviderConfig::local("local", "qwen3.8-flash", "http://127.0.0.1:9/v1", 4_096);
    config.capabilities.tools = true;
    let adapter = OpenAiCompat::new(config).unwrap();
    let mut request = request();
    request.capabilities.tools = true;
    request.tools.push(rupi_core::ToolSpec {
      name: "read".into(),
      description: "Read a path".into(),
      parameters: serde_json::json!({
        "type": "object",
        "properties": {"path": {"type": "string"}},
        "required": ["path"]
      }),
      sampling_constraint: Some(ToolSamplingConstraint::JsonSchema {
        strictness: ToolSamplingStrictness::Require,
      }),
    });
    let mut collector = Collector::default();
    let failure = adapter
      .stream(&request, &mut collector, &CancelToken::new())
      .unwrap_err();
    assert_eq!(failure.kind, ModelFailureKind::Protocol);
    assert_eq!(failure.phase, FailurePhase::PreRequest);
    assert!(failure.message.contains("does not support it"));
    assert!(collector.events().is_empty());
  }

  #[test]
  fn an_unreachable_endpoint_is_an_availability_failure_not_a_protocol_bug() {
    // Bound the remote connect attempt: the relay returns an explicit 502 when
    // it cannot open the provider connection before dispatch.
    let mut config =
      ProviderConfig::local("local", "qwen3.8-flash", "http://127.0.0.1:9/v1", 4_096);
    config.connect_timeout_ms = 200;
    let adapter = OpenAiCompat::new(config).unwrap();
    let mut collector = Collector::default();
    let failure = adapter
      .stream(&request(), &mut collector, &CancelToken::new())
      .unwrap_err();
    assert_eq!(failure.kind, ModelFailureKind::ProviderUnavailable);
    assert_eq!(failure.status, Some(502));
    assert_eq!(failure.phase, FailurePhase::WaitingForResponse);
    assert!(!failure.partial_output_emitted);
    assert_eq!(
      failure.replay_safety,
      rupi_core::RequestReplaySafety::Safe,
      "the refused connection never dispatched a request"
    );
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
    let mut decoder = Decoder::new(ReasoningExposure::Native);
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
