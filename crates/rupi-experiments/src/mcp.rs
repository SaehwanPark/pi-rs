//! MCP capability exposure optimization experiments and measurements.
//!
//! Canonical rules:
//! - Minimal exposure remains the default posture for rupi: tools are not exposed
//!   in context until specifically enabled or invoked.
//! - Predictive prefetch is evaluated as an opt-in mechanism to balance schema token overhead
//!   against prompt relevance.

use serde::{Deserialize, Serialize};

/// Summary of an MCP tool schema and estimated token footprint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpToolSchemaSummary {
  pub server_name: String,
  pub tool_name: String,
  pub schema_tokens: usize,
  pub keywords: Vec<String>,
}

impl McpToolSchemaSummary {
  pub fn new(
    server_name: impl Into<String>,
    tool_name: impl Into<String>,
    schema_tokens: usize,
    keywords: Vec<String>,
  ) -> Self {
    Self {
      server_name: server_name.into(),
      tool_name: tool_name.into(),
      schema_tokens,
      keywords,
    }
  }

  /// Calculates relevance score against a prompt text.
  pub fn relevance_score(&self, prompt: &str) -> f64 {
    let lower = prompt.to_lowercase();
    let mut matches = 0;
    for kw in &self.keywords {
      if lower.contains(&kw.to_lowercase()) {
        matches += 1;
      }
    }
    if self.keywords.is_empty() {
      0.0
    } else {
      (matches as f64) / (self.keywords.len() as f64)
    }
  }
}

/// Capability exposure strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExposureStrategy {
  /// Minimal/lazy exposure: zero MCP schemas in prompt context by default.
  Minimal,
  /// Predictive prefetch: expose tools whose relevance score exceeds a threshold.
  PredictivePrefetch,
  /// Eager full exposure: expose all configured MCP tools into prompt context.
  FullEager,
}

/// Evaluation result for an exposure strategy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExposureEvaluation {
  pub strategy: ExposureStrategy,
  pub exposed_tool_count: usize,
  pub total_schema_tokens: usize,
  pub context_overhead_pct: f64,
  pub estimated_coverage: f64,
}

/// Evaluates the token overhead and coverage across exposure strategies.
pub fn evaluate_mcp_exposure(
  tools: &[McpToolSchemaSummary],
  prompt: &str,
  working_context_target: u64,
) -> Vec<ExposureEvaluation> {
  let target = working_context_target.max(1_000) as f64;

  // 1. Minimal exposure: 0 schemas by default
  let minimal = ExposureEvaluation {
    strategy: ExposureStrategy::Minimal,
    exposed_tool_count: 0,
    total_schema_tokens: 0,
    context_overhead_pct: 0.0,
    estimated_coverage: 1.0, // Lazy resolution on demand provides full eventual coverage
  };

  // 2. Full eager exposure
  let total_eager_tokens: usize = tools.iter().map(|t| t.schema_tokens).sum();
  let full_eager = ExposureEvaluation {
    strategy: ExposureStrategy::FullEager,
    exposed_tool_count: tools.len(),
    total_schema_tokens: total_eager_tokens,
    context_overhead_pct: (total_eager_tokens as f64 / target) * 100.0,
    estimated_coverage: 1.0,
  };

  // 3. Predictive prefetch (relevance threshold > 0.25)
  let relevant_tools: Vec<_> = tools
    .iter()
    .filter(|t| t.relevance_score(prompt) >= 0.25)
    .collect();
  let predictive_tokens: usize = relevant_tools.iter().map(|t| t.schema_tokens).sum();
  let predictive = ExposureEvaluation {
    strategy: ExposureStrategy::PredictivePrefetch,
    exposed_tool_count: relevant_tools.len(),
    total_schema_tokens: predictive_tokens,
    context_overhead_pct: (predictive_tokens as f64 / target) * 100.0,
    estimated_coverage: if tools.is_empty() {
      1.0
    } else {
      (relevant_tools.len() as f64) / (tools.len() as f64)
    },
  };

  vec![minimal, predictive, full_eager]
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn minimal_exposure_has_zero_overhead() {
    let tools = vec![
      McpToolSchemaSummary::new("db", "query_sql", 450, vec!["sql".into(), "query".into()]),
      McpToolSchemaSummary::new("git", "git_blame", 350, vec!["git".into(), "commit".into()]),
      McpToolSchemaSummary::new(
        "jira",
        "create_issue",
        600,
        vec!["jira".into(), "issue".into()],
      ),
    ];

    let evals = evaluate_mcp_exposure(&tools, "inspect git history", 16_000);
    assert_eq!(evals[0].strategy, ExposureStrategy::Minimal);
    assert_eq!(evals[0].total_schema_tokens, 0);
    assert_eq!(evals[0].context_overhead_pct, 0.0);

    assert_eq!(evals[1].strategy, ExposureStrategy::PredictivePrefetch);
    assert_eq!(evals[1].exposed_tool_count, 1);
    assert_eq!(evals[1].total_schema_tokens, 350);

    assert_eq!(evals[2].strategy, ExposureStrategy::FullEager);
    assert_eq!(evals[2].exposed_tool_count, 3);
    assert_eq!(evals[2].total_schema_tokens, 1400);
    assert!(evals[2].context_overhead_pct > evals[1].context_overhead_pct);
  }
}
