//! Context lifecycle contracts.
//!
//! Canonical rule: **context is a cache, not the record.** The context engine
//! may shrink what the model sees, but it may never become the place where
//! history is kept, and it may never claim that a reduction lost information
//! that is still recoverable from the trace.
//!
//! Policy lives here as data. Mechanism (rewriting messages, archiving
//! payloads, writing capsules) lives in the context engine and store. The
//! ladder is:
//!
//! - [`ContextLevel::L0Payload`] — reduce an oversized payload to a bounded
//!   representation with a recovery reference.
//! - [`ContextLevel::L1Ordinary`] — ordinary compaction of older turns.
//! - [`ContextLevel::L2Phase`] — semantic phase compaction at a declared
//!   boundary, only at safe idle points, behind a cooldown.
//! - [`ContextLevel::L3Checkpoint`] — episode capsule plus reviewed reset.
//!
//! Thresholds shrink for constrained windows and deliberately do **not** grow
//! for large advertised windows: a provider offering a huge window has not
//! claimed that a huge working context is desirable.

use serde::{Deserialize, Serialize};

/// The four reduction levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextLevel {
  /// Payload-level reduction of one oversized item.
  #[serde(rename = "l0_payload")]
  L0Payload,
  /// Ordinary compaction of older conversation.
  #[serde(rename = "l1_ordinary")]
  L1Ordinary,
  /// Semantic phase compaction across a declared boundary.
  #[serde(rename = "l2_phase")]
  L2Phase,
  /// Episode checkpoint plus reviewed reset.
  #[serde(rename = "l3_checkpoint")]
  L3Checkpoint,
}

impl ContextLevel {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::L0Payload => "L0 payload reduction",
      Self::L1Ordinary => "L1 ordinary compaction",
      Self::L2Phase => "L2 phase compaction",
      Self::L3Checkpoint => "L3 checkpoint/reset",
    }
  }

  /// `true` when the level changes model-visible history structurally and so
  /// may only run at a safe boundary.
  pub fn requires_safe_boundary(self) -> bool {
    matches!(self, Self::L1Ordinary | Self::L2Phase | Self::L3Checkpoint)
  }
}

/// Why one payload was reduced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReductionReason {
  /// A single tool output exceeded the inline threshold.
  OversizedToolOutput { limit_bytes: u64 },
  /// The model-visible recent window exceeded its target.
  RecentTargetExceeded { target_tokens: u64 },
  /// MCP or external content was too large to keep inline.
  ExternalContextTooLarge { limit_bytes: u64 },
  /// The user asked for reduction.
  UserRequested,
}

/// What measured context looks like right now.
///
/// `measured_tokens` is preferred over `estimated_tokens`; when a provider
/// reports usage, the measurement wins, and estimates are only used before the
/// first response of a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextState {
  pub window: u64,
  pub estimated_tokens: u64,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub measured_tokens: Option<u64>,
  pub recent_tokens: u64,
  /// Messages currently in the model-visible working set.
  pub working_messages: u32,
  /// Increments on every structural compaction; used to make compaction epochs
  /// answerable.
  pub context_epoch: u32,
  /// `true` at a point where history may be rewritten: no request in flight,
  /// no tool call pending.
  pub at_safe_boundary: bool,
  /// Milliseconds since the last structural compaction.
  pub since_last_compaction_ms: u64,
  /// `true` when the previous request reported context overflow.
  pub overflow_observed: bool,
}

impl ContextState {
  /// Best available token count for this moment.
  pub fn effective_tokens(&self) -> u64 {
    self.measured_tokens.unwrap_or(self.estimated_tokens)
  }

  pub fn zero(window: u64) -> Self {
    Self {
      window,
      estimated_tokens: 0,
      measured_tokens: None,
      recent_tokens: 0,
      working_messages: 0,
      context_epoch: 0,
      at_safe_boundary: true,
      since_last_compaction_ms: 0,
      overflow_observed: false,
    }
  }
}

/// What the policy wants done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum ContextAction {
  /// Nothing to do.
  Keep,
  /// Continue, but surface pressure in the UI.
  Warn { tokens: u64, threshold: u64 },
  /// Reduce one payload at L0. Mechanism chooses which payload.
  ReducePayload { reason: ReductionReason },
  /// Compact at the given level. Only valid at a safe boundary.
  Compact { level: ContextLevel, reason: String },
  /// Recommend a checkpoint and reviewed reset.
  SuggestCheckpoint { reason: String },
  /// The active request cannot fit, even empty-handed. Refuse instead of
  /// silently truncating.
  Refuse { reason: String },
}

impl ContextAction {
  pub fn is_actionable(&self) -> bool {
    !matches!(self, Self::Keep)
  }
}

/// Evaluation result with the numbers that produced it.
///
/// Thresholds are decided, but a policy decision that cannot be audited later
/// is not debuggable, so the triggering numbers travel with the action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextDecision {
  pub action: ContextAction,
  pub tokens: u64,
  pub level: Option<ContextLevel>,
}

/// Policy interface. Mechanism is not allowed inside the policy.
pub trait ContextPolicy: Send + Sync {
  fn evaluate(&self, state: &ContextState) -> ContextDecision;
  /// Human-named profile in use, for `/context`.
  fn name(&self) -> &'static str;
}

/// Built-in profiles. `balanced` is the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextProfile {
  Aggressive,
  #[default]
  Balanced,
  Relaxed,
}

impl ContextProfile {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Aggressive => "aggressive",
      Self::Balanced => "balanced",
      Self::Relaxed => "relaxed",
    }
  }

  pub fn parse(value: &str) -> Option<Self> {
    match value.trim().to_ascii_lowercase().as_str() {
      "aggressive" => Some(Self::Aggressive),
      "balanced" => Some(Self::Balanced),
      "relaxed" => Some(Self::Relaxed),
      _ => None,
    }
  }

  fn fractions(self) -> ProfileFractions {
    match self {
      Self::Aggressive => ProfileFractions {
        warn: 0.45,
        reduce: 0.55,
        compact: 0.65,
        recent_target: 0.15,
        working_cap_tokens: 32_000,
      },
      Self::Balanced => ProfileFractions {
        warn: 0.60,
        reduce: 0.70,
        compact: 0.80,
        recent_target: 0.25,
        working_cap_tokens: 64_000,
      },
      Self::Relaxed => ProfileFractions {
        warn: 0.75,
        reduce: 0.82,
        compact: 0.90,
        recent_target: 0.35,
        working_cap_tokens: 96_000,
      },
    }
  }
}

/// A window small enough that thresholds shrink.
const CONSTRAINED_WINDOW_TOKENS: u64 = 64_000;
/// A window large enough that it must not relax thresholds.
const LARGE_WINDOW_TOKENS: u64 = 192_000;

#[derive(Debug, Clone, Copy)]
struct ProfileFractions {
  warn: f64,
  reduce: f64,
  compact: f64,
  recent_target: f64,
  working_cap_tokens: u64,
}

/// Absolute thresholds derived from a profile and one window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextThresholds {
  pub warn_tokens: u64,
  pub reduce_tokens: u64,
  pub compact_tokens: u64,
  pub checkpoint_tokens: u64,
  pub recent_target_tokens: u64,
}

impl ContextThresholds {
  /// Derive absolute thresholds for one window.
  ///
  /// The compact threshold is the anchor: the working cap limits how much
  /// context a profile may hold, and a constrained window lowers the anchor
  /// fraction so that headroom grows as a share of what is actually available.
  /// Warn and reduce scale off the anchor so the ladder never collapses.
  ///
  /// Windows above `LARGE_WINDOW_TOKENS` are treated as if they were exactly
  /// that large, and the cap still applies. That is how "a large advertised
  /// window does not imply a large working context" is enforced rather than
  /// merely stated.
  pub fn for_profile(profile: ContextProfile, window: u64) -> Self {
    let fractions = profile.fractions();
    let effective_window = window.min(LARGE_WINDOW_TOKENS) as f64;
    let constrained = window <= CONSTRAINED_WINDOW_TOKENS;
    let compact_fraction = fractions.compact - if constrained { 0.05 } else { 0.0 };
    let compact = (effective_window * compact_fraction).min(fractions.working_cap_tokens as f64);
    let scaled = |fraction: f64| (compact * (fraction / fractions.compact).clamp(0.05, 1.5)) as u64;
    Self {
      warn_tokens: scaled(fractions.warn),
      reduce_tokens: scaled(fractions.reduce),
      compact_tokens: compact as u64,
      // A small overshoot is allowed before recommending a checkpoint: waiting
      // for the next safe boundary is cheaper than resetting at the boundary.
      checkpoint_tokens: compact as u64 + (compact as u64 / 10).max(4_096),
      recent_target_tokens: ((effective_window.min(fractions.working_cap_tokens as f64))
        * fractions.recent_target) as u64,
    }
  }

  /// Cooldown before another structural compaction of the same session.
  ///
  /// Compaction is expensive and can thrash when a session keeps producing
  /// large output; the cooldown turns a thrash into a checkpoint suggestion.
  pub fn compaction_cooldown_ms(level: ContextLevel) -> u64 {
    match level {
      ContextLevel::L0Payload => 0,
      ContextLevel::L1Ordinary => 30_000,
      ContextLevel::L2Phase => 60_000,
      ContextLevel::L3Checkpoint => 120_000,
    }
  }
}

/// The built-in policy: profile thresholds plus explicit rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProfilePolicy {
  pub profile: ContextProfile,
  pub thresholds: ContextThresholds,
}

impl ProfilePolicy {
  pub fn new(profile: ContextProfile, window: u64) -> Self {
    Self {
      profile,
      thresholds: ContextThresholds::for_profile(profile, window),
    }
  }
}

impl ContextPolicy for ProfilePolicy {
  fn name(&self) -> &'static str {
    self.profile.as_str()
  }

  /// Ladder order: overflow, checkpoint, ordinary compaction, payload
  /// reduction, warn, keep. A structural action that is not allowed at the
  /// current boundary degrades to the next safe action rather than silently
  /// doing nothing, because "policy said compact, runtime did nothing" is the
  /// worst outcome for predictability.
  fn evaluate(&self, state: &ContextState) -> ContextDecision {
    let tokens = state.effective_tokens();
    let thresholds = self.thresholds;

    if state.overflow_observed {
      if tokens > thresholds.compact_tokens && state.at_safe_boundary {
        return ContextDecision {
          action: ContextAction::Compact {
            level: ContextLevel::L1Ordinary,
            reason: "provider reported context overflow".into(),
          },
          tokens,
          level: Some(ContextLevel::L1Ordinary),
        };
      }
      return ContextDecision {
        action: ContextAction::Refuse {
          reason: format!(
            "request exceeds the active context window ({tokens} > {} tokens); compact before continuing",
            thresholds.compact_tokens
          ),
        },
        tokens,
        level: None,
      };
    }

    if tokens >= thresholds.checkpoint_tokens {
      if state.at_safe_boundary
        && state.since_last_compaction_ms
          >= ContextThresholds::compaction_cooldown_ms(ContextLevel::L3Checkpoint)
      {
        return ContextDecision {
          action: ContextAction::SuggestCheckpoint {
            reason: format!(
              "working context at {} of {} tokens",
              tokens, thresholds.checkpoint_tokens
            ),
          },
          tokens,
          level: Some(ContextLevel::L3Checkpoint),
        };
      }
      return ContextDecision {
        action: ContextAction::Warn {
          tokens,
          threshold: thresholds.checkpoint_tokens,
        },
        tokens,
        level: None,
      };
    }

    if tokens >= thresholds.compact_tokens {
      if state.at_safe_boundary
        && state.since_last_compaction_ms
          >= ContextThresholds::compaction_cooldown_ms(ContextLevel::L1Ordinary)
      {
        return ContextDecision {
          action: ContextAction::Compact {
            level: ContextLevel::L1Ordinary,
            reason: format!(
              "context {} exceeds compact threshold {}",
              tokens, thresholds.compact_tokens
            ),
          },
          tokens,
          level: Some(ContextLevel::L1Ordinary),
        };
      }
      return ContextDecision {
        action: ContextAction::Warn {
          tokens,
          threshold: thresholds.compact_tokens,
        },
        tokens,
        level: None,
      };
    }

    if tokens >= thresholds.reduce_tokens || state.recent_tokens > thresholds.recent_target_tokens {
      return ContextDecision {
        action: ContextAction::ReducePayload {
          reason: ReductionReason::RecentTargetExceeded {
            target_tokens: thresholds.recent_target_tokens,
          },
        },
        tokens,
        level: Some(ContextLevel::L0Payload),
      };
    }

    if tokens >= thresholds.warn_tokens {
      return ContextDecision {
        action: ContextAction::Warn {
          tokens,
          threshold: thresholds.warn_tokens,
        },
        tokens,
        level: None,
      };
    }

    ContextDecision {
      action: ContextAction::Keep,
      tokens,
      level: None,
    }
  }
}

/// Versioned structured capsule.
///
/// Free-form summaries are not enough: a capsule must keep the objective, the
/// constraints, unresolved work, and next actions as separable fields, so that
/// a later model can act on them instead of re-reading prose.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCapsule {
  pub version: u32,
  pub objective: String,
  #[serde(default)]
  pub completed_work: Vec<String>,
  #[serde(default)]
  pub decisions: Vec<CapsuleDecision>,
  #[serde(default)]
  pub constraints: Vec<String>,
  pub current_state: String,
  #[serde(default)]
  pub artifacts: Vec<CapsuleArtifact>,
  #[serde(default)]
  pub unresolved: Vec<String>,
  #[serde(default)]
  pub next_actions: Vec<String>,
}

/// Current capsule schema version.
pub const CAPSULE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapsuleDecision {
  pub decision: String,
  /// Why this decision was made, as stated at the time.
  pub rationale: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapsuleArtifact {
  pub path: String,
  pub note: String,
}

#[cfg(test)]
mod tests {
  use super::*;

  fn state(tokens: u64) -> ContextState {
    ContextState {
      since_last_compaction_ms: 600_000,
      ..ContextState::zero(128_000).with_tokens(tokens)
    }
  }

  impl ContextState {
    fn with_tokens(mut self, tokens: u64) -> Self {
      self.estimated_tokens = tokens;
      self
    }
  }

  #[test]
  fn thresholds_shrink_for_small_windows_and_cap_large_ones() {
    let small = ContextThresholds::for_profile(ContextProfile::Balanced, 16_000);
    let big = ContextThresholds::for_profile(ContextProfile::Balanced, 1_000_000);
    assert!(
      small.compact_tokens < 16_000,
      "small windows must leave headroom: {}",
      small.compact_tokens
    );
    assert!(
      big.compact_tokens <= 64_000,
      "large windows must not raise the working cap: {}",
      big.compact_tokens
    );
    assert!(small.warn_tokens <= small.compact_tokens);
    assert!(small.compact_tokens <= small.checkpoint_tokens);
  }

  #[test]
  fn aggressive_compacts_before_balanced() {
    let window = 128_000;
    let aggressive = ContextThresholds::for_profile(ContextProfile::Aggressive, window);
    let balanced = ContextThresholds::for_profile(ContextProfile::Balanced, window);
    let relaxed = ContextThresholds::for_profile(ContextProfile::Relaxed, window);
    assert!(aggressive.compact_tokens < balanced.compact_tokens);
    assert!(balanced.compact_tokens < relaxed.compact_tokens);
  }

  #[test]
  fn quiet_context_keeps() {
    let policy = ProfilePolicy::new(ContextProfile::Balanced, 128_000);
    let decision = policy.evaluate(&state(1_000));
    assert_eq!(decision.action, ContextAction::Keep);
  }

  #[test]
  fn pressure_orders_warn_reduce_compact_checkpoint() {
    let policy = ProfilePolicy::new(ContextProfile::Balanced, 128_000);
    let t = policy.thresholds;

    let warn = policy.evaluate(&ContextState {
      recent_tokens: 0,
      ..state(t.warn_tokens + 1)
    });
    assert!(
      matches!(warn.action, ContextAction::Warn { .. }),
      "{warn:?}"
    );

    let reduce = policy.evaluate(&ContextState {
      recent_tokens: t.recent_target_tokens + 1,
      ..state(t.reduce_tokens + 1)
    });
    assert!(
      matches!(reduce.action, ContextAction::ReducePayload { .. }),
      "{reduce:?}"
    );

    let compact = policy.evaluate(&state(t.compact_tokens + 1));
    assert!(
      matches!(
        compact.action,
        ContextAction::Compact {
          level: ContextLevel::L1Ordinary,
          ..
        }
      ),
      "{compact:?}"
    );

    let checkpoint = policy.evaluate(&state(t.checkpoint_tokens + 1));
    assert!(
      matches!(checkpoint.action, ContextAction::SuggestCheckpoint { .. }),
      "{checkpoint:?}"
    );
  }

  #[test]
  fn structural_compaction_waits_for_a_safe_boundary() {
    let policy = ProfilePolicy::new(ContextProfile::Balanced, 128_000);
    let busy = ContextState {
      at_safe_boundary: false,
      ..state(policy.thresholds.compact_tokens + 1)
    };
    let decision = policy.evaluate(&busy);
    assert!(
      matches!(decision.action, ContextAction::Warn { .. }),
      "mid-request history rewriting is never allowed: {decision:?}"
    );
    assert!(ContextLevel::L1Ordinary.requires_safe_boundary());
  }

  #[test]
  fn cooldown_turns_repeated_pressure_into_a_warning() {
    let policy = ProfilePolicy::new(ContextProfile::Balanced, 128_000);
    let recently_compacted = ContextState {
      since_last_compaction_ms: 1_000,
      ..state(policy.thresholds.compact_tokens + 1)
    };
    let decision = policy.evaluate(&recently_compacted);
    assert!(
      matches!(decision.action, ContextAction::Warn { .. }),
      "thrashing must be damped: {decision:?}"
    );
  }

  #[test]
  fn overflow_compacts_at_a_boundary_and_refuses_otherwise() {
    let policy = ProfilePolicy::new(ContextProfile::Balanced, 128_000);
    let overflow = ContextState {
      overflow_observed: true,
      ..state(policy.thresholds.compact_tokens + 1)
    };
    assert!(
      matches!(
        policy.evaluate(&overflow).action,
        ContextAction::Compact { .. }
      ),
      "overflow at a boundary is recoverable"
    );

    let mid_request = ContextState {
      at_safe_boundary: false,
      ..overflow.clone()
    };
    assert!(
      matches!(
        policy.evaluate(&mid_request).action,
        ContextAction::Refuse { .. }
      ),
      "overflow mid-request must be refused, never silently truncated"
    );
  }

  #[test]
  fn capsule_schema_is_versioned() {
    let capsule = ContextCapsule {
      version: CAPSULE_SCHEMA_VERSION,
      objective: "ship phase 1".into(),
      completed_work: vec!["core contracts".into()],
      decisions: vec![CapsuleDecision {
        decision: "single store crate".into(),
        rationale: "shared layout and redaction".into(),
      }],
      constraints: vec!["no network on startup path".into()],
      current_state: "provider adapter next".into(),
      artifacts: vec![CapsuleArtifact {
        path: "crates/pi-rs-provider".into(),
        note: "openai-compatible".into(),
      }],
      unresolved: vec!["failover".into()],
      next_actions: vec!["implement tools".into()],
    };
    let encoded = serde_json::to_string(&capsule).unwrap();
    let decoded: ContextCapsule = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, capsule);
    assert_eq!(decoded.version, CAPSULE_SCHEMA_VERSION);
  }

  #[test]
  fn profile_names_round_trip() {
    for profile in [
      ContextProfile::Aggressive,
      ContextProfile::Balanced,
      ContextProfile::Relaxed,
    ] {
      assert_eq!(ContextProfile::parse(profile.as_str()), Some(profile));
    }
    assert_eq!(ContextProfile::parse("moderate"), None);
    assert_eq!(ContextProfile::default(), ContextProfile::Balanced);
  }
}
