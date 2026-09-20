//! Optimization experiments and measurements for rupi.
//!
//! Covers:
//! - Context adaptation: prefill latency tracking, performance knee detection,
//!   adaptive threshold capping, and static-profile comparison.
//! - Backup optimization: warm standby evaluation and startup/RSS memory trade-off measurement.
//! - MCP optimization: predictive capability prefetch modeling and schema exposure measurement.

pub mod backup;
pub mod context;
pub mod mcp;

pub use backup::{
  BackupMode, StandbyMeasurement, StandbyTradeoffAnalysis, evaluate_standby_tradeoff,
};
pub use context::{
  AdaptiveContextPolicy, KneeDetector, KneePoint, LatencySample, ProfileComparison,
};
pub use mcp::{ExposureEvaluation, ExposureStrategy, McpToolSchemaSummary, evaluate_mcp_exposure};
