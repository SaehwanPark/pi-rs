//! Typed provider failure classes.
//!
//! Failure handling is split in two places on purpose:
//!
//! - this module decides *what kind* of failure happened and whether the kind
//!   is in principle retryable or failover-eligible;
//! - the runtime decides *whether this occurrence* may be retried or may
//!   trigger takeover, using retry budget, committed tool state, and the
//!   capability gate.
//!
//! Quality is never an availability failure. A mediocre answer, a failing
//! test, or an apparently confused model must not move the session to a backup
//! model, so those cases are [`ModelFailureKind::Semantic`] and are neither
//! retried nor failed over.

use serde::{Deserialize, Serialize};

use crate::capability::ModelRef;

/// Normalized failure category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelFailureKind {
  /// Connection could not be established or dropped: refused, reset, DNS,
  /// interrupted stream.
  Transport,
  /// Deadline exceeded.
  Timeout,
  /// Explicit throttle response.
  RateLimited,
  /// Provider or local server answered but cannot serve the request now,
  /// including 5xx and a missing model endpoint.
  ProviderUnavailable,
  /// Credentials rejected or not authorized for this model.
  Authentication,
  /// Provider spoke a protocol we could not use: truncated JSON, unknown
  /// event shape, unusable tool-call encoding, or a request we got wrong.
  Protocol,
  /// The request does not fit the active context window.
  ContextOverflow,
  /// The model answered, and the answer is not usable. Not an availability
  /// failure.
  Semantic,
  /// The user or harness cancelled.
  Cancelled,
}

impl ModelFailureKind {
  /// Stable machine label for trace and diagnostics.
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Transport => "transport",
      Self::Timeout => "timeout",
      Self::RateLimited => "rate_limited",
      Self::ProviderUnavailable => "provider_unavailable",
      Self::Authentication => "authentication",
      Self::Protocol => "protocol",
      Self::ContextOverflow => "context_overflow",
      Self::Semantic => "semantic",
      Self::Cancelled => "cancelled",
    }
  }

  /// Whether retrying the same request against the same model can plausibly
  /// succeed.
  pub fn is_retryable(self) -> bool {
    matches!(
      self,
      Self::Transport | Self::Timeout | Self::RateLimited | Self::ProviderUnavailable
    )
  }

  /// Whether this kind may make the backup model take over *after* retries are
  /// exhausted.
  ///
  /// Being a candidate is not permission to take over. The runtime still has
  /// to exhaust retries, pass the capability gate, and preserve committed tool
  /// state.
  ///
  /// [`ModelFailureKind::Authentication`] is intentionally excluded: our
  /// credentials being wrong is not something a second model can be assumed to
  /// fix, and silently switching providers on an auth error hides the real
  /// problem from the user.
  ///
  /// [`ModelFailureKind::ContextOverflow`] is excluded because the remedy is
  /// compaction or an explicit refusal, not a different model.
  pub fn is_failover_candidate(self) -> bool {
    matches!(
      self,
      Self::Transport
        | Self::Timeout
        | Self::RateLimited
        | Self::ProviderUnavailable
        | Self::Protocol
    )
  }
}

impl std::fmt::Display for ModelFailureKind {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.as_str())
  }
}

/// Where in a request lifecycle the failure happened.
///
/// Needed for mid-turn failover: a failure after partial output was already
/// committed is handled differently from a failure before anything was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailurePhase {
  /// Before the request left the runtime.
  PreRequest,
  /// Request sent, no usable response yet.
  WaitingForResponse,
  /// Response body was being streamed.
  Streaming,
  /// Stream finished, but normalization failed.
  Normalizing,
}

/// A normalized provider failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelFailure {
  pub kind: ModelFailureKind,
  pub phase: FailurePhase,
  /// Operator-facing one-line description. Provider text is summarized, never
  /// trusted as structure.
  pub message: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub status: Option<u16>,
  /// Server-provided hint, or a runtime backoff suggestion, in milliseconds.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub retry_after_ms: Option<u64>,
  /// Attempts already spent against the same model for the same request.
  #[serde(default)]
  pub attempts: u32,
  /// `true` when output from this request was already surfaced and committed.
  ///
  /// A streamed assistant message is committed content: after that point the
  /// runtime must continue rather than restart the request.
  #[serde(default)]
  pub partial_output_emitted: bool,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub model: Option<ModelRef>,
  /// Bounded, redaction-eligible provider detail kept for diagnosis. Never
  /// used for control flow.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub detail: Option<String>,
}

impl ModelFailure {
  pub fn new(kind: ModelFailureKind, phase: FailurePhase, message: impl Into<String>) -> Self {
    Self {
      kind,
      phase,
      message: message.into(),
      status: None,
      retry_after_ms: None,
      attempts: 0,
      partial_output_emitted: false,
      model: None,
      detail: None,
    }
  }

  pub fn with_status(mut self, status: u16) -> Self {
    self.status = Some(status);
    self
  }

  pub fn with_retry_after_ms(mut self, ms: u64) -> Self {
    self.retry_after_ms = Some(ms);
    self
  }

  pub fn with_attempts(mut self, attempts: u32) -> Self {
    self.attempts = attempts;
    self
  }

  pub fn with_partial_output(mut self, emitted: bool) -> Self {
    self.partial_output_emitted = emitted;
    self
  }

  pub fn with_model(mut self, model: ModelRef) -> Self {
    self.model = Some(model);
    self
  }

  pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
    self.detail = Some(detail.into());
    self
  }

  /// Whether replaying the failed request is permitted.
  ///
  /// This is the single rule the runtime should consult rather than
  /// re-deriving it from scattered fields. Two conditions must hold: the
  /// failure is an availability failure (or an explicit override says so), and
  /// the caller has not already seen output.
  ///
  /// The second condition is the one that is easy to get wrong: replaying after
  /// deltas were emitted duplicates visible text, and replaying past a tool
  /// boundary risks repeating a mutation. A caller that has already surfaced
  /// output must fail the turn, or fail over, instead of silently retrying.
  pub fn safe_to_retry(&self) -> bool {
    self.kind.is_retryable() && !self.partial_output_emitted
  }

  /// Map an HTTP status plus a short provider message to a failure kind.
  ///
  /// Context overflow is checked first because providers commonly report it as
  /// a 400, and treating it as a protocol error would wrongly classify an
  /// in-principle recoverable condition.
  pub fn classify_http(status: u16, message: &str) -> ModelFailureKind {
    if looks_like_context_overflow(message) {
      return ModelFailureKind::ContextOverflow;
    }
    match status {
      401 | 403 => ModelFailureKind::Authentication,
      408 | 504 => ModelFailureKind::Timeout,
      429 => ModelFailureKind::RateLimited,
      404 | 410 => ModelFailureKind::ProviderUnavailable,
      413 => ModelFailureKind::ContextOverflow,
      500..=599 => ModelFailureKind::ProviderUnavailable,
      // 4xx from us: the request or the capability claim is wrong. Retrying
      // the identical request cannot help, and a backup model is not assumed
      // to accept it either.
      400..=499 => ModelFailureKind::Protocol,
      _ => ModelFailureKind::Protocol,
    }
  }
}

impl std::fmt::Display for ModelFailure {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "{}", self.kind)?;
    if let Some(status) = self.status {
      write!(f, " (http {status})")?;
    }
    write!(f, ": {}", self.message)
  }
}

fn looks_like_context_overflow(message: &str) -> bool {
  let lowered = message.to_ascii_lowercase();
  lowered.contains("context length")
    || lowered.contains("context_length")
    || lowered.contains("maximum context")
    || lowered.contains("context window")
    || lowered.contains("context size")
    || lowered.contains("exceeds the supported")
    || lowered.contains("too many tokens")
    || lowered.contains("prompt is too long")
    || lowered.contains("reduce the length of the messages")
}

/// Classification of tool-completion certainty, shared by the tool runtime and
/// failover continuity.
///
/// `Unknown` is a terminal state in its own right. It is never coerced into
/// success or failure, because the difference decides whether a side effect
/// may be replayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionCertainty {
  /// Completion was observed.
  Certain,
  /// Completion could not be observed: the boundary is uncertain.
  Unknown,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn http_status_maps_to_availability_classes() {
    assert_eq!(
      ModelFailure::classify_http(429, "slow down"),
      ModelFailureKind::RateLimited
    );
    assert_eq!(
      ModelFailure::classify_http(503, "no capacity"),
      ModelFailureKind::ProviderUnavailable
    );
    assert_eq!(
      ModelFailure::classify_http(404, "model not found"),
      ModelFailureKind::ProviderUnavailable
    );
    assert_eq!(
      ModelFailure::classify_http(408, "deadline"),
      ModelFailureKind::Timeout
    );
    assert_eq!(
      ModelFailure::classify_http(401, "bad key"),
      ModelFailureKind::Authentication
    );
    assert_eq!(
      ModelFailure::classify_http(400, "invalid json in tool args"),
      ModelFailureKind::Protocol
    );
  }

  #[test]
  fn context_overflow_beats_generic_bad_request() {
    for message in [
      "This model's maximum context length is 8192 tokens",
      "prompt is too long: 40000 tokens",
      "request exceeds context window",
      // llama.cpp and llama-server phrase it this way, and treating it as a
      // generic 400 would hide the one overflow a local runtime actually hits.
      // Captured from llama.cpp llama-server, with its real error type
      // (`exceed_context_size_error`) in the body.
      "request (300010 tokens) exceeds the available context size (131072 tokens), try increasing it",
      "Please reduce the length of the messages or completion",
    ] {
      assert_eq!(
        ModelFailure::classify_http(400, message),
        ModelFailureKind::ContextOverflow,
        "{message}"
      );
    }
  }

  #[test]
  fn auth_and_overflow_are_never_failover_candidates() {
    for kind in [
      ModelFailureKind::Authentication,
      ModelFailureKind::ContextOverflow,
      ModelFailureKind::Semantic,
      ModelFailureKind::Cancelled,
    ] {
      assert!(!kind.is_failover_candidate(), "{kind}");
      assert!(!kind.is_retryable(), "{kind}");
    }
  }

  #[test]
  fn protocol_is_retryable_only_via_candidate_path() {
    // Protocol is a takeover candidate after repeated failures, but a single
    // protocol error is not retried blindly: the retry loop uses
    // `is_retryable`, takeover uses `is_failover_candidate`.
    assert!(ModelFailureKind::Protocol.is_failover_candidate());
    assert!(!ModelFailureKind::Protocol.is_retryable());
  }

  #[test]
  fn a_mid_turn_failure_keeps_phase_and_partial_output_separate_from_kind() {
    // The three facts answer three different questions: `kind` is whether the
    // provider is at fault and worth retrying; `phase` is where the boundary
    // sits; `partial_output_emitted` is whether the user already saw something.
    let interrupted = ModelFailure::new(
      ModelFailureKind::Transport,
      FailurePhase::Streaming,
      "connection reset mid-stream",
    )
    .with_partial_output(true);
    assert_eq!(interrupted.kind, ModelFailureKind::Transport);
    assert!(interrupted.kind.is_retryable());
    assert!(
      !interrupted.safe_to_retry(),
      "replay would duplicate visible text"
    );
    assert!(interrupted.partial_output_emitted);

    let before_request = ModelFailure::new(
      ModelFailureKind::Transport,
      FailurePhase::WaitingForResponse,
      "connection refused",
    );
    assert!(
      before_request.safe_to_retry(),
      "nothing was sent, so retry is safe"
    );
    assert!(!before_request.partial_output_emitted);

    // A model that answered badly is not an availability failure, and a
    // cancelled turn is never retried.
    let rejected = ModelFailure::new(
      ModelFailureKind::Semantic,
      FailurePhase::Normalizing,
      "model produced an incoherent answer",
    );
    assert!(!rejected.kind.is_retryable());
    assert!(!rejected.safe_to_retry());
    let cancelled = ModelFailure::new(
      ModelFailureKind::Cancelled,
      FailurePhase::Streaming,
      "user aborted",
    )
    .with_partial_output(true);
    assert_eq!(cancelled.kind, ModelFailureKind::Cancelled);
    assert!(!cancelled.safe_to_retry());
  }

  #[test]
  fn retry_after_survives_the_round_trip_as_a_duration() {
    let failure = ModelFailure::new(
      ModelFailureKind::RateLimited,
      FailurePhase::WaitingForResponse,
      "slow down",
    )
    .with_retry_after_ms(2_500);
    let decoded: ModelFailure =
      serde_json::from_str(&serde_json::to_string(&failure).unwrap()).unwrap();
    assert_eq!(decoded.retry_after_ms, Some(2_500));
    let decoded: ModelFailure = serde_json::from_str(
      &serde_json::to_string(&ModelFailure::new(
        ModelFailureKind::Transport,
        FailurePhase::PreRequest,
        "no hint",
      ))
      .unwrap(),
    )
    .unwrap();
    assert_eq!(decoded.retry_after_ms, None);
  }

  #[test]
  fn failure_round_trips_with_optional_fields() {
    let failure = ModelFailure::new(
      ModelFailureKind::Transport,
      FailurePhase::Streaming,
      "stream interrupted",
    )
    .with_attempts(2)
    .with_partial_output(true)
    .with_model(ModelRef::new("local", "qwen"));
    let encoded = serde_json::to_string(&failure).unwrap();
    let decoded: ModelFailure = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, failure);
    assert!(
      encoded.contains("\"partial_output_emitted\":true"),
      "{encoded}"
    );
  }
}
