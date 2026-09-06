//! A backup model that costs nothing until a takeover actually needs it.
//!
//! Failover is rare and startup is not. A configured backup must therefore not
//! buy a TLS agent, resolve a credential environment variable, or open any
//! socket while the session is starting, because the overwhelming majority of
//! sessions never use it. This wrapper keeps the two facts the runtime needs
//! before takeover — which model this is, and what it declared it can do — and
//! builds the real adapter at the first request that is actually addressed to it.
//!
//! The declared capability snapshot is what the failover gate compares against.
//! That is a deliberate difference from a live provider, whose capabilities are
//! read from itself: a deferred backup has no live anything to read, so the
//! operator's declaration is the only available claim, and the switch decision is
//! made from it rather than from a probe that would defeat the point.
//!
//! A build failure is remembered, not retried: a credential that is missing from
//! the environment does not appear between attempts, and re-running a failing
//! builder would turn one honest error into a stream of them.

use std::{fmt, sync::OnceLock};

use pi_rs_core::{
  CancelToken, CompletionUsage, FailurePhase, ModelCapabilities, ModelFailure, ModelFailureKind,
  ModelProvider, ModelRef, ModelRequest, ProviderEventSink,
};

/// Builds the real adapter for a backup model.
pub type Builder = dyn Fn() -> Result<Box<dyn ModelProvider>, String> + Send + Sync;

/// A [`ModelProvider`] that constructs its inner adapter on first use.
pub struct Deferred {
  model: ModelRef,
  capabilities: ModelCapabilities,
  build: Box<Builder>,
  built: OnceLock<Result<Box<dyn ModelProvider>, String>>,
}

impl Deferred {
  /// Wrap a builder with what the runtime must know before it exists.
  pub fn new(
    model: ModelRef,
    capabilities: ModelCapabilities,
    build: impl Fn() -> Result<Box<dyn ModelProvider>, String> + Send + Sync + 'static,
  ) -> Self {
    Self {
      model,
      capabilities,
      build: Box::new(build),
      built: OnceLock::new(),
    }
  }

  /// Whether the inner adapter has been constructed.
  ///
  /// A backup that was never needed must never be initialized; tests pin that.
  pub fn is_initialized(&self) -> bool {
    self.built.get().is_some()
  }

  /// The initialization failure, if the builder already failed.
  pub fn init_error(&self) -> Option<&str> {
    match self.built.get() {
      Some(Err(reason)) => Some(reason),
      _ => None,
    }
  }

  fn inner(&self) -> Result<&dyn ModelProvider, &str> {
    // `OnceLock::get_or_init` runs the builder exactly once even under
    // concurrent first requests, and memoises both success and failure.
    self
      .built
      .get_or_init(|| (self.build)())
      .as_deref()
      .map_err(|reason| reason.as_str())
  }
}

impl ModelProvider for Deferred {
  fn provider_id(&self) -> &str {
    &self.model.provider
  }

  fn model(&self) -> &ModelRef {
    &self.model
  }

  /// The declared snapshot. Never initializes the inner adapter.
  fn capabilities(&self) -> ModelCapabilities {
    self.capabilities.clone()
  }

  fn stream(
    &self,
    request: &ModelRequest,
    sink: &mut dyn ProviderEventSink,
    cancel: &CancelToken,
  ) -> Result<CompletionUsage, ModelFailure> {
    match self.inner() {
      Ok(provider) => provider.stream(request, sink, cancel),
      // The runtime already decided to trust this model, so an adapter that
      // cannot be built is an availability failure of that model, reported
      // before the request left. Silently returning to the model that just
      // failed over would be worse than one honest error.
      Err(reason) => Err(ModelFailure::new(
        ModelFailureKind::ProviderUnavailable,
        FailurePhase::PreRequest,
        format!("{} could not be initialized: {reason}", self.model),
      )),
    }
  }
}

impl fmt::Debug for Deferred {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("Deferred")
      .field("model", &self.model.as_key())
      .field("initialized", &self.is_initialized())
      .finish_non_exhaustive()
  }
}

#[cfg(test)]
mod tests {
  use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
  };

  use super::*;

  #[derive(Debug)]
  struct Echo {
    model: ModelRef,
  }

  impl ModelProvider for Echo {
    fn provider_id(&self) -> &str {
      &self.model.provider
    }
    fn model(&self) -> &ModelRef {
      &self.model
    }
    fn capabilities(&self) -> ModelCapabilities {
      ModelCapabilities::text_only(4_096)
    }
    fn stream(
      &self,
      _request: &ModelRequest,
      _sink: &mut dyn ProviderEventSink,
      _cancel: &CancelToken,
    ) -> Result<CompletionUsage, ModelFailure> {
      Ok(CompletionUsage {
        input_tokens: None,
        output_tokens: None,
        finish_reason: Some("stop".into()),
        certainty: pi_rs_core::CompletionCertainty::Certain,
      })
    }
  }

  fn caps() -> ModelCapabilities {
    ModelCapabilities::text_only(8_192)
  }

  fn deferred(builds: Arc<AtomicU32>) -> Deferred {
    Deferred::new(ModelRef::new("backup", "model"), caps(), move || {
      builds.fetch_add(1, Ordering::SeqCst);
      Ok(Box::new(Echo {
        model: ModelRef::new("backup", "model"),
      }))
    })
  }

  fn request() -> ModelRequest {
    ModelRequest::new(
      ModelRef::new("backup", "model"),
      caps(),
      vec![pi_rs_core::Message::user("go")],
    )
  }

  #[derive(Default)]
  struct Quiet;
  impl ProviderEventSink for Quiet {
    fn emit(&mut self, _event: &pi_rs_core::ProviderEvent) {}
  }

  #[test]
  fn nothing_is_built_before_the_backup_is_asked() {
    let builds = Arc::new(AtomicU32::new(0));
    let provider = deferred(builds.clone());
    // The runtime reads these two before takeover: identity for the epoch record,
    // capabilities for the gate. Neither may construct anything.
    assert_eq!(provider.model().as_key(), "backup/model");
    assert_eq!(provider.provider_id(), "backup");
    assert_eq!(provider.capabilities().context_window, 8_192);
    assert_eq!(builds.load(Ordering::SeqCst), 0);
    assert!(!provider.is_initialized());
  }

  #[test]
  fn first_request_builds_once_and_later_requests_reuse() {
    let builds = Arc::new(AtomicU32::new(0));
    let provider = deferred(builds.clone());
    let cancel = CancelToken::new();
    for _ in 0..3 {
      provider
        .stream(&request(), &mut Quiet, &cancel)
        .expect("serves");
    }
    assert_eq!(builds.load(Ordering::SeqCst), 1);
    assert!(provider.is_initialized());
  }

  #[test]
  fn a_build_failure_is_reported_once_and_remembered() {
    let builds = Arc::new(AtomicU32::new(0));
    let builds_in = builds.clone();
    let provider = Deferred::new(ModelRef::new("backup", "gone"), caps(), move || {
      builds_in.fetch_add(1, Ordering::SeqCst);
      Err("BACKUP_TOKEN is not set".into())
    });
    let cancel = CancelToken::new();
    for _ in 0..2 {
      let failure = provider
        .stream(&request(), &mut Quiet, &cancel)
        .expect_err("builder fails");
      assert_eq!(failure.kind, ModelFailureKind::ProviderUnavailable);
      assert_eq!(failure.phase, FailurePhase::PreRequest);
      assert!(
        failure
          .message
          .contains("backup/gone could not be initialized")
      );
      assert!(failure.message.contains("BACKUP_TOKEN is not set"));
    }
    assert_eq!(
      builds.load(Ordering::SeqCst),
      1,
      "a builder that failed must not be retried within the turn"
    );
    assert_eq!(provider.init_error(), Some("BACKUP_TOKEN is not set"));
  }

  #[test]
  fn debug_output_names_the_model_without_claiming_a_connection() {
    let builds = Arc::new(AtomicU32::new(0));
    let text = format!("{:?}", deferred(builds.clone()));
    assert!(text.contains("backup/model"));
    assert!(text.contains("initialized: false"));
    assert_eq!(builds.load(Ordering::SeqCst), 0);
  }
}
