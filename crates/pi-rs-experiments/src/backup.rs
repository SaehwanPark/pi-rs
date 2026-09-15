//! Backup standby optimization experiments and trade-off measurements.
//!
//! Canonical rules:
//! - Cold lazy backup remains the default posture for pi-rs.
//! - Backup adapter initialization must not slow down normal startup (<100 ms warm, <250 ms cold).
//! - Optional warm standby is evaluated as a measurable trade-off against startup and memory budgets.

use serde::{Deserialize, Serialize};

/// Mode of backup model initialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupMode {
  /// Adapter initialization is strictly deferred until a primary failure triggers failover.
  #[default]
  ColdLazy,
  /// Adapter and credentials are instantiated eagerly during process startup.
  WarmStandby,
}

impl BackupMode {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::ColdLazy => "cold_lazy",
      Self::WarmStandby => "warm_standby",
    }
  }
}

/// Standby measurement profile for a given mode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StandbyMeasurement {
  pub mode: BackupMode,
  /// Added startup latency in milliseconds.
  pub startup_overhead_ms: f64,
  /// Added resident memory overhead in bytes.
  pub memory_overhead_bytes: u64,
  /// Estimated time to first token on failover takeover in milliseconds.
  pub failover_takeover_ms: f64,
}

/// Trade-off analysis comparing cold lazy backup and warm standby.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StandbyTradeoffAnalysis {
  pub cold: StandbyMeasurement,
  pub warm: StandbyMeasurement,
  /// Startup penalty introduced by warm standby in milliseconds.
  pub startup_penalty_ms: f64,
  /// Memory penalty introduced by warm standby in bytes.
  pub memory_penalty_bytes: u64,
  /// Failover speedup provided by warm standby in milliseconds.
  pub failover_speedup_ms: f64,
  /// Explicit repository architectural recommendation.
  pub recommendation: &'static str,
}

/// Evaluates the startup and memory trade-offs of warm standby vs cold lazy backup.
pub fn evaluate_standby_tradeoff(
  warm_init_cost_ms: f64,
  warm_memory_bytes: u64,
  failover_init_cost_ms: f64,
) -> StandbyTradeoffAnalysis {
  let cold = StandbyMeasurement {
    mode: BackupMode::ColdLazy,
    startup_overhead_ms: 0.0,
    memory_overhead_bytes: 0,
    failover_takeover_ms: failover_init_cost_ms + 250.0,
  };

  let warm = StandbyMeasurement {
    mode: BackupMode::WarmStandby,
    startup_overhead_ms: warm_init_cost_ms,
    memory_overhead_bytes: warm_memory_bytes,
    failover_takeover_ms: 250.0,
  };

  let startup_penalty = warm.startup_overhead_ms - cold.startup_overhead_ms;
  let memory_penalty = warm.memory_overhead_bytes - cold.memory_overhead_bytes;
  let failover_speedup = cold.failover_takeover_ms - warm.failover_takeover_ms;

  StandbyTradeoffAnalysis {
    cold,
    warm,
    startup_penalty_ms: startup_penalty,
    memory_penalty_bytes: memory_penalty,
    failover_speedup_ms: failover_speedup,
    recommendation: "Cold lazy backup remains the default because startup latency (<100ms warm budget) and lean memory must be protected for the vast majority of runs that never experience primary failover.",
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn cold_backup_has_zero_startup_and_memory_overhead() {
    let analysis = evaluate_standby_tradeoff(15.0, 1_048_576, 18.0);
    assert_eq!(analysis.cold.startup_overhead_ms, 0.0);
    assert_eq!(analysis.cold.memory_overhead_bytes, 0);
    assert!(analysis.startup_penalty_ms > 0.0);
    assert!(analysis.memory_penalty_bytes > 0);
    assert!(analysis.failover_speedup_ms > 0.0);
    assert!(
      analysis
        .recommendation
        .contains("Cold lazy backup remains the default")
    );
  }
}
