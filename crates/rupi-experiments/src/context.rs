//! Context adaptation and latency knee detection contracts.
//!
//! Canonical rules:
//! - Context adaptation is experimental and opt-in; static profile behavior remains default.
//! - Adaptive thresholds may only lower or cap profile-derived thresholds, never increase them.
//! - Compaction must trigger before entering model-specific performance knees/cliffs.

use serde::{Deserialize, Serialize};

use rupi_core::context::{
  ContextAction, ContextDecision, ContextLevel, ContextPolicy, ContextProfile, ContextState,
  ContextThresholds, ProfilePolicy, ReductionReason,
};
use rupi_core::{ContextOverrides, ModelRef};

/// A recorded latency observation for a model request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LatencySample {
  /// Context size in tokens.
  pub tokens: u64,
  /// Time to first token / first delta in milliseconds.
  pub first_delta_ms: u64,
  /// Full request duration in milliseconds.
  pub duration_ms: u64,
  /// Identifier of the model that produced this observation.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub model: Option<String>,
}

impl LatencySample {
  pub fn new(tokens: u64, first_delta_ms: u64, duration_ms: u64) -> Self {
    Self {
      tokens,
      first_delta_ms,
      duration_ms,
      model: None,
    }
  }

  pub fn with_model(mut self, model: impl Into<String>) -> Self {
    self.model = Some(model.into());
    self
  }
}

/// Detected performance knee point in the context-latency curve.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct KneePoint {
  /// Token boundary where latency begins accelerating non-linearly.
  pub tokens: u64,
  /// Marginal latency slope (ms per 1,000 tokens) before the knee point.
  pub slope_before: f64,
  /// Marginal latency slope (ms per 1,000 tokens) after the knee point.
  pub slope_after: f64,
  /// Ratio of slope_after to slope_before (acceleration factor).
  pub acceleration_ratio: f64,
}

/// Detector for identifying model/runtime-specific latency knees.
#[derive(Debug, Clone)]
pub struct KneeDetector {
  samples: Vec<LatencySample>,
  min_samples: usize,
  ratio_threshold: f64,
}

impl Default for KneeDetector {
  fn default() -> Self {
    Self {
      samples: Vec::new(),
      min_samples: 4,
      ratio_threshold: 1.8,
    }
  }
}

impl KneeDetector {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn with_min_samples(mut self, min_samples: usize) -> Self {
    self.min_samples = min_samples;
    self
  }

  pub fn with_ratio_threshold(mut self, ratio_threshold: f64) -> Self {
    self.ratio_threshold = ratio_threshold;
    self
  }

  pub fn add_sample(&mut self, sample: LatencySample) {
    self.samples.push(sample);
  }

  pub fn add_samples(&mut self, samples: impl IntoIterator<Item = LatencySample>) {
    self.samples.extend(samples);
  }

  pub fn samples(&self) -> &[LatencySample] {
    &self.samples
  }

  /// Detects whether a performance knee exists in the collected samples.
  ///
  /// The detector sorts samples by token count, groups or takes adjacent intervals,
  /// computes piecewise slopes (Δ first_delta_ms / Δ tokens * 1000), and flags a knee
  /// where the forward marginal slope surges significantly above the preceding baseline.
  pub fn detect_knee(&self) -> Option<KneePoint> {
    let model = self.samples.first()?.model.as_deref();
    if self
      .samples
      .iter()
      .any(|sample| sample.model.as_deref() != model)
    {
      return None;
    }
    self.detect_knee_for_model(model)
  }

  /// Detect a knee from observations belonging to exactly one model identity.
  ///
  /// `None` selects only unscoped samples; it never mixes them with named models.
  pub fn detect_knee_for_model(&self, model: Option<&str>) -> Option<KneePoint> {
    let samples: Vec<_> = self
      .samples
      .iter()
      .filter(|sample| sample.model.as_deref() == model)
      .cloned()
      .collect();
    if samples.len() < self.min_samples {
      return None;
    }

    let mut sorted = samples;
    sorted.sort_by_key(|s| s.tokens);
    // Dedup adjacent matching token counts by averaging latencies
    let mut deduped: Vec<LatencySample> = Vec::new();
    for sample in sorted {
      if let Some(last) = deduped.last_mut() {
        if last.tokens == sample.tokens {
          last.first_delta_ms = (last.first_delta_ms + sample.first_delta_ms) / 2;
          last.duration_ms = (last.duration_ms + sample.duration_ms) / 2;
          continue;
        }
      }
      deduped.push(sample);
    }

    if deduped.len() < self.min_samples {
      return None;
    }

    // Compute segment slopes between adjacent points
    let mut segment_slopes = Vec::new();
    for i in 0..deduped.len() - 1 {
      let d_tokens = deduped[i + 1].tokens.saturating_sub(deduped[i].tokens);
      if d_tokens == 0 {
        continue;
      }
      let d_ms = (deduped[i + 1].first_delta_ms as f64) - (deduped[i].first_delta_ms as f64);
      let slope = (d_ms / (d_tokens as f64)) * 1000.0;
      segment_slopes.push((deduped[i + 1].tokens, slope));
    }

    if segment_slopes.len() < 2 {
      return None;
    }

    // Look for the most prominent slope change
    let mut best_knee: Option<KneePoint> = None;
    let mut max_ratio = self.ratio_threshold;

    for i in 1..segment_slopes.len() {
      // Mean slope of preceding segments
      let sum_before: f64 = segment_slopes[0..i].iter().map(|(_, s)| s).sum();
      let slope_before = (sum_before / (i as f64)).max(0.01);
      let slope_after = segment_slopes[i].1;

      if slope_after <= slope_before {
        continue;
      }

      let ratio = slope_after / slope_before;
      if ratio >= max_ratio {
        max_ratio = ratio;
        let knee_tokens = segment_slopes[i - 1].0;
        best_knee = Some(KneePoint {
          tokens: knee_tokens,
          slope_before,
          slope_after,
          acceleration_ratio: ratio,
        });
      }
    }

    best_knee
  }
}

/// Comparison between static profile thresholds and adaptive thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileComparison {
  pub static_thresholds: ContextThresholds,
  pub adaptive_thresholds: ContextThresholds,
  pub knee_tokens: Option<u64>,
  pub compact_reduction_tokens: u64,
  pub adaptive_active: bool,
}

/// Adaptive context policy that caps thresholds when a performance knee is observed.
#[derive(Debug, Clone)]
pub struct AdaptiveContextPolicy {
  base_policy: ProfilePolicy,
  adaptive_thresholds: ContextThresholds,
  knee: Option<KneePoint>,
  knee_model: Option<String>,
  enabled: bool,
}

impl AdaptiveContextPolicy {
  pub fn new(profile: ContextProfile, window: u64, enabled: bool) -> Self {
    let base_policy = ProfilePolicy::new(profile, window);
    let static_thresholds = base_policy.thresholds;
    Self {
      base_policy,
      adaptive_thresholds: static_thresholds,
      knee: None,
      knee_model: None,
      enabled,
    }
  }

  /// Apply explicit operator thresholds before any adaptive knee cap.
  pub fn with_overrides(mut self, overrides: ContextOverrides) -> Self {
    self.base_policy = self.base_policy.with_overrides(overrides);
    self.adaptive_thresholds = self
      .base_policy
      .thresholds_for_window(self.base_policy.reference_window());
    if let Some(knee) = self.knee {
      self.apply_knee(knee);
    }
    self
  }

  /// Adaptively adjust thresholds using observations from a knee detector.
  ///
  /// INVARIANT: Adaptive thresholds only lower/cap static thresholds.
  /// If the knee token count exceeds static compact_tokens, static thresholds remain.
  pub fn with_detector(mut self, detector: &KneeDetector) -> Self {
    if !self.enabled {
      return self;
    }

    let Some(first) = detector.samples().first() else {
      return self;
    };
    let model = first.model.as_deref();
    if detector
      .samples()
      .iter()
      .any(|sample| sample.model.as_deref() != model)
    {
      return self;
    }
    if let Some(knee) = detector.detect_knee_for_model(model) {
      self.knee_model = model.map(str::to_owned);
      self.apply_knee(knee);
    }
    self
  }

  /// Attach observations for a specific model, ignoring other model samples.
  pub fn with_detector_for_model(mut self, detector: &KneeDetector, model: &ModelRef) -> Self {
    if !self.enabled {
      return self;
    }
    let key = model.as_key();
    if let Some(knee) = detector.detect_knee_for_model(Some(&key)) {
      self.knee_model = Some(key);
      self.apply_knee(knee);
    }
    self
  }

  fn apply_knee(&mut self, knee: KneePoint) {
    self.knee = Some(knee);
    let window = self.base_policy.reference_window();
    self.adaptive_thresholds = cap_thresholds(self.base_policy.thresholds_for_window(window), knee)
      .normalized_for_window(window);
  }

  pub fn is_enabled(&self) -> bool {
    self.enabled
  }

  pub fn detected_knee(&self) -> Option<KneePoint> {
    self.knee
  }

  pub fn effective_thresholds(&self) -> ContextThresholds {
    if self.enabled && self.knee.is_some() {
      self.adaptive_thresholds
    } else {
      self
        .base_policy
        .thresholds_for_window(self.base_policy.reference_window())
    }
  }

  pub fn compare_with_static(&self) -> ProfileComparison {
    let static_t = self
      .base_policy
      .thresholds_for_window(self.base_policy.reference_window());
    let eff_t = self.effective_thresholds();
    let reduction = static_t.compact_tokens.saturating_sub(eff_t.compact_tokens);
    ProfileComparison {
      static_thresholds: static_t,
      adaptive_thresholds: eff_t,
      knee_tokens: self.knee.map(|k| k.tokens),
      compact_reduction_tokens: reduction,
      adaptive_active: self.enabled && self.knee.is_some() && reduction > 0,
    }
  }
}

impl ContextPolicy for AdaptiveContextPolicy {
  fn name(&self) -> &'static str {
    if self.enabled && self.knee.is_some() {
      "adaptive"
    } else {
      self.base_policy.profile.as_str()
    }
  }

  fn configuration_warning(&self, state: &ContextState) -> Option<String> {
    self.base_policy.configuration_warning(state)
  }

  fn evaluate(&self, state: &ContextState) -> ContextDecision {
    let tokens = state.effective_tokens();
    if state.window < 3 {
      return ContextDecision {
        action: ContextAction::Refuse {
          reason: "active context window is too small to form valid policy thresholds".into(),
        },
        tokens,
        level: None,
      };
    }
    let static_thresholds = self.base_policy.thresholds_for_window(state.window);
    let active_model = state.model.as_ref().map(ModelRef::as_key);
    let thresholds = match (self.knee, active_model.as_ref() == self.knee_model.as_ref()) {
      (Some(knee), true) if self.enabled => {
        cap_thresholds(static_thresholds, knee).normalized_for_window(state.window)
      }
      _ => static_thresholds,
    };

    if state.overflow_observed {
      return ContextDecision {
        action: ContextAction::Compact {
          level: ContextLevel::L1Ordinary,
          reason: "uncommitted context overflow; compacting pre-turn history".into(),
          target_tokens: thresholds.recent_target_tokens,
        },
        tokens,
        level: Some(ContextLevel::L1Ordinary),
      };
    }

    if tokens >= thresholds.checkpoint_tokens {
      if state.at_safe_boundary {
        return ContextDecision {
          action: ContextAction::SuggestCheckpoint {
            reason: format!(
              "context tokens ({tokens}) exceeded checkpoint threshold ({})",
              thresholds.checkpoint_tokens
            ),
          },
          tokens,
          level: Some(ContextLevel::L3Checkpoint),
        };
      } else {
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
    }

    if tokens >= thresholds.compact_tokens {
      if state.at_safe_boundary {
        return ContextDecision {
          action: ContextAction::Compact {
            level: ContextLevel::L1Ordinary,
            reason: format!(
              "context tokens ({tokens}) exceeded compact threshold ({})",
              thresholds.compact_tokens
            ),
            target_tokens: thresholds.recent_target_tokens,
          },
          tokens,
          level: Some(ContextLevel::L1Ordinary),
        };
      } else {
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
    }

    if tokens >= thresholds.reduce_tokens {
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

fn cap_thresholds(static_thresholds: ContextThresholds, knee: KneePoint) -> ContextThresholds {
  if knee.tokens >= static_thresholds.compact_tokens {
    return static_thresholds;
  }

  let cap = knee.tokens;
  let scale = cap as f64 / static_thresholds.compact_tokens as f64;
  ContextThresholds {
    warn_tokens: ((static_thresholds.warn_tokens as f64) * scale) as u64,
    reduce_tokens: ((static_thresholds.reduce_tokens as f64) * scale) as u64,
    compact_tokens: cap,
    checkpoint_tokens: static_thresholds
      .checkpoint_tokens
      .min(cap.saturating_add((cap / 10).max(2_048))),
    recent_target_tokens: ((static_thresholds.recent_target_tokens as f64) * scale) as u64,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn adaptive_knee_does_not_raise_any_static_or_overridden_threshold() {
    let static_thresholds = ContextThresholds {
      warn_tokens: 1_000,
      reduce_tokens: 20_000,
      compact_tokens: 50_000,
      checkpoint_tokens: 50_500,
      recent_target_tokens: 40_000,
    };
    let knee = KneePoint {
      tokens: 49_000,
      slope_before: 1.0,
      slope_after: 3.0,
      acceleration_ratio: 3.0,
    };
    let adaptive = cap_thresholds(static_thresholds, knee).normalized_for_window(128_000);

    assert!(adaptive.warn_tokens <= static_thresholds.warn_tokens);
    assert!(adaptive.reduce_tokens <= static_thresholds.reduce_tokens);
    assert!(adaptive.compact_tokens <= static_thresholds.compact_tokens);
    assert!(adaptive.checkpoint_tokens <= static_thresholds.checkpoint_tokens);
    assert!(adaptive.recent_target_tokens <= static_thresholds.recent_target_tokens);
  }

  #[test]
  fn linear_curve_has_no_knee() {
    let mut detector = KneeDetector::new();
    // Latency grows strictly linearly: 50ms per 1k tokens
    for i in 1..=8 {
      let tokens = i * 4_000;
      let ms = i * 200;
      detector.add_sample(LatencySample::new(tokens, ms, ms + 100));
    }
    assert_eq!(detector.detect_knee(), None);
  }

  #[test]
  fn detects_sharp_knee() {
    let mut detector = KneeDetector::new();
    // Fast flat region up to 32k tokens
    detector.add_sample(LatencySample::new(8_000, 100, 300));
    detector.add_sample(LatencySample::new(16_000, 120, 350));
    detector.add_sample(LatencySample::new(24_000, 140, 400));
    detector.add_sample(LatencySample::new(32_000, 160, 450));
    // Steep quadratic cliff after 32k tokens
    detector.add_sample(LatencySample::new(48_000, 600, 1200));
    detector.add_sample(LatencySample::new(64_000, 1500, 2500));

    let knee = detector.detect_knee().expect("knee should be detected");
    assert_eq!(knee.tokens, 32_000);
    assert!(knee.acceleration_ratio >= 2.0);
  }

  #[test]
  fn adaptive_policy_caps_and_never_exceeds_static_profile() {
    let window = 128_000;
    let mut detector = KneeDetector::new();
    detector.add_sample(LatencySample::new(8_000, 100, 200));
    detector.add_sample(LatencySample::new(16_000, 110, 220));
    detector.add_sample(LatencySample::new(24_000, 120, 240));
    detector.add_sample(LatencySample::new(32_000, 130, 260));
    // Knee at 32k
    detector.add_sample(LatencySample::new(48_000, 550, 900));

    let static_policy = ProfilePolicy::new(ContextProfile::Balanced, window);
    let adaptive_policy =
      AdaptiveContextPolicy::new(ContextProfile::Balanced, window, true).with_detector(&detector);

    let comp = adaptive_policy.compare_with_static();
    assert!(comp.adaptive_active);
    assert_eq!(comp.knee_tokens, Some(32_000));
    assert!(comp.adaptive_thresholds.compact_tokens <= 32_000);
    assert!(comp.adaptive_thresholds.compact_tokens < static_policy.thresholds.compact_tokens);
    assert!(comp.adaptive_thresholds.reduce_tokens < static_policy.thresholds.reduce_tokens);
    assert!(comp.adaptive_thresholds.warn_tokens < static_policy.thresholds.warn_tokens);
  }

  #[test]
  fn adaptive_knee_caps_explicit_overrides_without_raising_them() {
    let window = 128_000;
    let mut detector = KneeDetector::new();
    detector.add_sample(LatencySample::new(8_000, 100, 200));
    detector.add_sample(LatencySample::new(16_000, 110, 220));
    detector.add_sample(LatencySample::new(24_000, 120, 240));
    detector.add_sample(LatencySample::new(32_000, 130, 260));
    detector.add_sample(LatencySample::new(48_000, 550, 900));
    let overrides = ContextOverrides {
      warn_tokens: Some(10_000),
      reduce_tokens: Some(20_000),
      compact_tokens: Some(50_000),
      checkpoint_tokens: Some(60_000),
      recent_target_tokens: Some(8_000),
    };
    let policy = AdaptiveContextPolicy::new(ContextProfile::Balanced, window, true)
      .with_overrides(overrides)
      .with_detector(&detector);
    let comparison = policy.compare_with_static();

    assert_eq!(comparison.static_thresholds.compact_tokens, 50_000);
    assert_eq!(comparison.knee_tokens, Some(32_000));
    assert!(comparison.adaptive_thresholds.compact_tokens <= 32_000);
    assert!(
      comparison.adaptive_thresholds.compact_tokens < comparison.static_thresholds.compact_tokens
    );
    assert!(
      comparison.adaptive_thresholds.warn_tokens <= comparison.adaptive_thresholds.reduce_tokens
    );
    assert!(
      comparison.adaptive_thresholds.reduce_tokens <= comparison.adaptive_thresholds.compact_tokens
    );
    assert!(
      comparison.adaptive_thresholds.compact_tokens
        < comparison.adaptive_thresholds.checkpoint_tokens
    );
    assert!(comparison.adaptive_thresholds.checkpoint_tokens < window);
    assert!(
      comparison.adaptive_thresholds.recent_target_tokens
        < comparison.adaptive_thresholds.compact_tokens
    );
  }

  #[test]
  fn adaptive_knees_are_scoped_to_the_active_model_and_window() {
    let large_model = ModelRef::new("cloud", "large");
    let backup_model = ModelRef::new("local", "small");
    let mut detector = KneeDetector::new();
    for (tokens, latency) in [
      (1_000, 100),
      (2_000, 110),
      (4_000, 120),
      (8_000, 130),
      (16_000, 550),
      (24_000, 1_500),
    ] {
      detector.add_sample(
        LatencySample::new(tokens, latency, latency + 100).with_model(large_model.as_key()),
      );
    }
    for (tokens, latency) in [
      (8_000, 100),
      (16_000, 110),
      (24_000, 120),
      (32_000, 130),
      (48_000, 550),
    ] {
      detector.add_sample(
        LatencySample::new(tokens, latency, latency + 100).with_model(backup_model.as_key()),
      );
    }
    assert_eq!(
      detector.detect_knee(),
      None,
      "different models are not pooled"
    );

    let overrides = ContextOverrides {
      warn_tokens: Some(2_000),
      reduce_tokens: Some(4_000),
      compact_tokens: Some(10_000),
      checkpoint_tokens: Some(12_000),
      recent_target_tokens: Some(5_000),
    };
    let policy = AdaptiveContextPolicy::new(ContextProfile::Balanced, 262_144, true)
      .with_overrides(overrides)
      .with_detector_for_model(&detector, &large_model);
    let large_state = ContextState {
      model: Some(large_model),
      estimated_tokens: 9_000,
      recent_tokens: 0,
      since_last_compaction_ms: 600_000,
      ..ContextState::zero(262_144)
    };
    assert!(matches!(
      policy.evaluate(&large_state).action,
      ContextAction::Compact { .. }
    ));

    let small_window = 32_768;
    let (small_thresholds, _) =
      ContextThresholds::for_profile(ContextProfile::Balanced, small_window)
        .with_overrides(&overrides, small_window);
    assert_eq!(small_thresholds.compact_tokens, 10_000);
    let backup_state = ContextState {
      model: Some(backup_model),
      estimated_tokens: small_thresholds.compact_tokens + 1,
      recent_tokens: 0,
      since_last_compaction_ms: 600_000,
      ..ContextState::zero(small_window)
    };
    assert!(matches!(
      policy.evaluate(&backup_state).action,
      ContextAction::Compact { .. }
    ));
  }

  #[test]
  fn disabled_adaptive_policy_matches_static_thresholds() {
    let window = 128_000;
    let mut detector = KneeDetector::new();
    detector.add_sample(LatencySample::new(8_000, 100, 200));
    detector.add_sample(LatencySample::new(16_000, 110, 220));
    detector.add_sample(LatencySample::new(24_000, 120, 240));
    detector.add_sample(LatencySample::new(32_000, 130, 260));
    detector.add_sample(LatencySample::new(48_000, 550, 900));

    // When disabled (default), thresholds remain static
    let adaptive_policy =
      AdaptiveContextPolicy::new(ContextProfile::Balanced, window, false).with_detector(&detector);
    let comp = adaptive_policy.compare_with_static();
    assert!(!comp.adaptive_active);
    assert_eq!(comp.adaptive_thresholds, comp.static_thresholds);
  }
}
