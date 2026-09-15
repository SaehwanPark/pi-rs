//! Context adaptation and latency knee detection contracts.
//!
//! Canonical rules:
//! - Context adaptation is experimental and opt-in; static profile behavior remains default.
//! - Adaptive thresholds may only lower or cap profile-derived thresholds, never increase them.
//! - Compaction must trigger before entering model-specific performance knees/cliffs.

use serde::{Deserialize, Serialize};

use pi_rs_core::context::{
  ContextAction, ContextDecision, ContextLevel, ContextPolicy, ContextProfile, ContextState,
  ContextThresholds, ProfilePolicy, ReductionReason,
};

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
    if self.samples.len() < self.min_samples {
      return None;
    }

    let mut sorted = self.samples.clone();
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
      enabled,
    }
  }

  /// Adaptively adjust thresholds using observations from a knee detector.
  ///
  /// INVARIANT: Adaptive thresholds only lower/cap static thresholds.
  /// If the knee token count exceeds static compact_tokens, static thresholds remain.
  pub fn with_detector(mut self, detector: &KneeDetector) -> Self {
    if !self.enabled {
      return self;
    }

    if let Some(knee) = detector.detect_knee() {
      self.knee = Some(knee);
      let static_thresh = self.base_policy.thresholds;

      // Only cap if the knee occurs before our current compact threshold
      if knee.tokens < static_thresh.compact_tokens {
        let cap = knee.tokens;
        let scale = cap as f64 / static_thresh.compact_tokens as f64;

        self.adaptive_thresholds = ContextThresholds {
          warn_tokens: ((static_thresh.warn_tokens as f64) * scale) as u64,
          reduce_tokens: ((static_thresh.reduce_tokens as f64) * scale) as u64,
          compact_tokens: cap,
          checkpoint_tokens: cap + (cap / 10).max(2_048),
          recent_target_tokens: ((static_thresh.recent_target_tokens as f64) * scale) as u64,
        };
      }
    }
    self
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
      self.base_policy.thresholds
    }
  }

  pub fn compare_with_static(&self) -> ProfileComparison {
    let static_t = self.base_policy.thresholds;
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

  fn evaluate(&self, state: &ContextState) -> ContextDecision {
    let tokens = state.effective_tokens();
    let thresholds = self.effective_thresholds();

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

#[cfg(test)]
mod tests {
  use super::*;

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
