//! Failover policy: when another model may take over generation.
//!
//! Failover is *fault recovery*, not orchestration. The rule that shapes this
//! module is that quality is never a reason: a confused or wrong answer from the
//! primary is still the primary's answer. Only defined availability failures
//! qualify, and only after transient ones have been retried against the model that
//! was actually asked.
//!
//! Ordering matters as much as the set. A 429 is retried before takeover because
//! the same model may serve it a second later. Context overflow never triggers
//! takeover, because a second model does not fix a request that is simply too big
//! — compaction does, and that is the context engine's job.

use pi_rs_core::{CapabilityGap, ModelCapabilities, ModelFailureKind, ModelRef};

/// What the runtime decided about a failed request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Recovery {
  /// Try again against the same model.
  Retry { attempt: u32, max_attempts: u32 },
  /// Ask the backup model. `gaps` are what the switch costs, and are recorded so
  /// the user can see that takeover was not free.
  Failover {
    to: ModelRef,
    gaps: Vec<CapabilityGap>,
  },
  /// Stop and report. Nothing further is safe to try automatically.
  Abort,
}

/// Which model may be used for recovery, and under what conditions.
pub struct FailoverPolicy {
  /// Backup model. `None` disables failover: every qualifying failure aborts.
  pub backup: Option<ModelRef>,
  /// Backup's capability snapshot, used for the preflight gate. Kept beside the
  /// reference so a policy can be evaluated without contacting the provider,
  /// which matters on the startup path.
  pub backup_capabilities: Option<ModelCapabilities>,
  /// Attempts against one model before considering recovery.
  pub max_attempts: u32,
  /// What the *session* needs, as a capability snapshot. The backup must be able
  /// to do the work already in flight; failing over into a second, different
  /// failure is worse than one honest failure.
  pub required: ModelCapabilities,
}

impl Default for FailoverPolicy {
  fn default() -> Self {
    Self {
      backup: None,
      backup_capabilities: None,
      // Two attempts: the first failure is often a dropped socket, and one retry
      // covers that without turning a hung endpoint into a long stall.
      max_attempts: 2,
      required: ModelCapabilities::text_only(8_192),
    }
  }
}

impl FailoverPolicy {
  /// Attach a backup model with the capabilities believed at wiring time.
  pub fn with_backup(mut self, model: ModelRef, capabilities: ModelCapabilities) -> Self {
    self.backup = Some(model);
    self.backup_capabilities = Some(capabilities);
    self
  }

  /// Declare what the session requires, usually the primary's own snapshot.
  pub fn requiring(mut self, required: ModelCapabilities) -> Self {
    self.required = required;
    self
  }

  /// Set the per-model attempt budget.
  pub fn with_max_attempts(mut self, attempts: u32) -> Self {
    self.max_attempts = attempts.max(1);
    self
  }

  /// Decide the next action for a failure.
  ///
  /// `attempt` counts requests already made against the *current* model for this
  /// turn, starting at 1.
  pub fn decide(
    &self,
    kind: ModelFailureKind,
    attempt: u32,
    partial_output_emitted: bool,
  ) -> Recovery {
    // A cancelled turn is not a fault to recover from: the user asked us to stop,
    // and switching models in order to keep going would override that.
    if matches!(kind, ModelFailureKind::Cancelled) {
      return Recovery::Abort;
    }

    // Once output has been streamed, restarting would duplicate committed content
    // in the transcript. Continuing across a model boundary is a different
    // feature from retrying, so this aborts rather than inventing it here.
    if partial_output_emitted {
      return Recovery::Abort;
    }

    // Retry transient availability failures against the model that was asked,
    // until the attempt budget is spent.
    if kind.is_retryable() && attempt < self.max_attempts {
      return Recovery::Retry {
        attempt,
        max_attempts: self.max_attempts,
      };
    }

    if kind.is_failover_candidate() {
      return self.failover_or_abort();
    }

    // Overflow, authentication, and semantic failures do not become someone
    // else's problem by asking another model.
    Recovery::Abort
  }

  fn failover_or_abort(&self) -> Recovery {
    let (Some(backup), Some(capabilities)) =
      (self.backup.clone(), self.backup_capabilities.as_ref())
    else {
      return Recovery::Abort;
    };
    let gaps = capabilities.gaps(&self.required);
    if ModelCapabilities::has_hard_gap(&gaps) {
      // The backup cannot do this work at all. Refuse the switch.
      return Recovery::Abort;
    }
    // A context-window gap alone is not a blocker: compaction closes it, and the
    // caller is told which gaps remain so the cost of takeover stays visible.
    Recovery::Failover { to: backup, gaps }
  }
}

#[cfg(test)]
mod tests {
  use pi_rs_core::capability::ReasoningExposure;

  use super::*;

  fn model(id: &str) -> ModelRef {
    ModelRef::new("backup", id)
  }

  fn caps(tools: bool, window: u64) -> ModelCapabilities {
    ModelCapabilities {
      text: true,
      images: false,
      tools,
      exposed_reasoning: ReasoningExposure::None,
      context_window: window,
      max_output_tokens: Some(4096),
    }
  }

  /// Primary that calls tools in a 128k window.
  fn tool_use_primary() -> ModelCapabilities {
    caps(true, 128_000)
  }

  fn backup(tools: bool, window: u64) -> FailoverPolicy {
    FailoverPolicy::default()
      .with_backup(model("backup"), caps(tools, window))
      .requiring(tool_use_primary())
  }

  #[test]
  fn quality_failures_never_trigger_failover() {
    // The central prohibition: a bad answer is not an availability failure.
    for kind in [ModelFailureKind::Semantic, ModelFailureKind::Authentication] {
      let decision = backup(true, 128_000).decide(kind, 1, false);
      assert_eq!(decision, Recovery::Abort, "{kind:?} must not recover");
    }
  }

  #[test]
  fn transient_failures_retry_the_same_model_first() {
    let policy = backup(true, 128_000).with_max_attempts(3);
    for kind in [
      ModelFailureKind::Transport,
      ModelFailureKind::Timeout,
      ModelFailureKind::RateLimited,
      ModelFailureKind::ProviderUnavailable,
    ] {
      let decision = policy.decide(kind, 1, false);
      assert_eq!(
        decision,
        Recovery::Retry {
          attempt: 1,
          max_attempts: 3
        },
        "{kind:?} should retry first"
      );
    }
  }

  #[test]
  fn retries_are_spent_before_takeover() {
    let policy = backup(true, 128_000).with_max_attempts(2);
    let decision = policy.decide(ModelFailureKind::Transport, 2, false);
    assert!(
      matches!(decision, Recovery::Failover { .. }),
      "after the budget: {decision:?}"
    );
  }

  #[test]
  fn streaming_output_forbids_a_retry() {
    // Restarting after output was committed would duplicate it in the transcript.
    let policy = backup(true, 128_000).with_max_attempts(5);
    let decision = policy.decide(ModelFailureKind::Transport, 1, true);
    assert_eq!(
      decision,
      Recovery::Abort,
      "committed output may not be replayed"
    );
  }

  #[test]
  fn cancellation_is_not_a_fault() {
    let decision = backup(true, 128_000).decide(ModelFailureKind::Cancelled, 1, false);
    assert_eq!(decision, Recovery::Abort, "the user said stop");
  }

  #[test]
  fn no_backup_means_no_failover() {
    let policy = FailoverPolicy::default()
      .with_max_attempts(1)
      .requiring(tool_use_primary());
    let decision = policy.decide(ModelFailureKind::ProviderUnavailable, 1, false);
    assert_eq!(decision, Recovery::Abort);
  }

  #[test]
  fn a_backup_that_cannot_do_the_work_is_not_used() {
    let policy = backup(false, 128_000).with_max_attempts(1);
    let decision = policy.decide(ModelFailureKind::ProviderUnavailable, 1, false);
    assert_eq!(
      decision,
      Recovery::Abort,
      "a backup without tool calling trades one failure for a silent regression"
    );
  }

  #[test]
  fn a_narrower_backup_may_still_take_over() {
    // A window shortfall is recoverable by compaction, so it is reported as a gap
    // rather than as a refusal.
    let policy = backup(true, 32_000).with_max_attempts(1);
    match policy.decide(ModelFailureKind::ProviderUnavailable, 1, false) {
      Recovery::Failover { gaps, .. } => assert!(
        gaps
          .iter()
          .any(|gap| matches!(gap, CapabilityGap::ContextWindow { .. }))
      ),
      other => panic!("expected failover with a reported gap, got {other:?}"),
    }
  }

  #[test]
  fn context_overflow_is_not_a_takeover_trigger() {
    // The remedy is compaction. Switching models would hide the real fix.
    let policy = backup(true, 1_000_000).with_max_attempts(1);
    let decision = policy.decide(ModelFailureKind::ContextOverflow, 1, false);
    assert_eq!(decision, Recovery::Abort);
  }

  #[test]
  fn a_protocol_failure_moves_without_retrying() {
    // Protocol is a failover candidate but not retryable: replaying the same
    // request to the same endpoint would most likely fail the same way, so the
    // only useful recovery left is a different model.
    let policy = backup(true, 128_000).with_max_attempts(4);
    let decision = policy.decide(ModelFailureKind::Protocol, 1, false);
    assert!(
      matches!(decision, Recovery::Failover { .. }),
      "{decision:?}"
    );
    // Without a backup, the same failure aborts rather than looping.
    let alone = FailoverPolicy::default()
      .with_max_attempts(4)
      .requiring(tool_use_primary());
    assert_eq!(
      alone.decide(ModelFailureKind::Protocol, 1, false),
      Recovery::Abort
    );
  }

  #[test]
  fn attempt_budget_is_respected() {
    let policy = backup(true, 128_000).with_max_attempts(4);
    for attempt in 1..4 {
      assert!(
        matches!(
          policy.decide(ModelFailureKind::RateLimited, attempt, false),
          Recovery::Retry { .. }
        ),
        "attempt {attempt} should still retry"
      );
    }
    assert!(
      matches!(
        policy.decide(ModelFailureKind::RateLimited, 4, false),
        Recovery::Failover { .. }
      ),
      "attempt 4 must stop retrying"
    );
  }

  #[test]
  fn an_attempt_budget_of_zero_is_not_possible() {
    // Zero would mean "never try", which would make every turn fail before the
    // first request. The minimum is clamped instead.
    let policy = FailoverPolicy::default().with_max_attempts(0);
    assert_eq!(policy.max_attempts, 1);
  }
}
