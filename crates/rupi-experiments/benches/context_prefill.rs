//! Context prefill and optimization experiments benchmark.
//!
//! Measures:
//! - Prefill latency curve evaluation and knee detection.
//! - Static profile vs adaptive threshold calculation and decision evaluation.
//! - Backup standby trade-off calculation.
//! - MCP schema exposure evaluation.
//!
//! Run: `cargo bench -p rupi-experiments --bench context_prefill`
//! Or:  `bench/context_prefill.sh [--iterations <N>] [--json <path>]`

use std::{env, fs, process::ExitCode, time::Instant};

use rupi_core::context::{ContextPolicy, ContextProfile, ContextState};
use rupi_experiments::{
  AdaptiveContextPolicy, KneeDetector, LatencySample, McpToolSchemaSummary, evaluate_mcp_exposure,
  evaluate_standby_tradeoff,
};

/// Latency budgets in microseconds (us) per benchmark case.
const BUDGETS: &[(&str, f64)] = &[
  ("knee_detection_curve_6_points", 1_000.0),
  ("adaptive_policy_evaluation", 500.0),
  ("static_vs_adaptive_comparison", 500.0),
  ("standby_tradeoff_evaluation", 500.0),
  ("mcp_exposure_evaluation", 500.0),
];

const DEFAULT_ITERATIONS: usize = 100;

fn arg_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
  let mut iter = args.iter();
  while let Some(arg) = iter.next() {
    if arg == flag {
      return iter.next().map(|s| s.as_str());
    }
    if let Some(rest) = arg.strip_prefix(&format!("{flag}=")) {
      return Some(rest);
    }
  }
  None
}

fn median(samples: &mut [f64]) -> f64 {
  samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
  let mid = samples.len() / 2;
  if samples.len() % 2 == 0 {
    (samples[mid - 1] + samples[mid]) / 2.0
  } else {
    samples[mid]
  }
}

fn generate_synthetic_samples() -> Vec<LatencySample> {
  vec![
    LatencySample::new(4_000, 80, 200),
    LatencySample::new(8_000, 100, 250),
    LatencySample::new(16_000, 130, 320),
    LatencySample::new(32_000, 190, 480),
    LatencySample::new(48_000, 650, 1200), // Knee starts here
    LatencySample::new(64_000, 1600, 2800),
  ]
}

fn main() -> ExitCode {
  let args: Vec<String> = env::args().skip(1).collect();
  if args.iter().any(|a| a == "--help" || a == "-h") {
    println!("Usage: context_prefill [--iterations <N>] [--json <path>]");
    return ExitCode::SUCCESS;
  }
  let iterations = arg_value(&args, "--iterations")
    .and_then(|v| v.parse::<usize>().ok())
    .filter(|n| *n > 0)
    .unwrap_or(DEFAULT_ITERATIONS);
  let json_out = arg_value(&args, "--json").map(String::from);

  let samples = generate_synthetic_samples();
  let mut measured: Vec<(&str, f64)> = Vec::new();

  println!("Context prefill and optimization benchmarks (rupi-experiments):");

  // 1. Knee detection
  {
    let name = "knee_detection_curve_6_points";
    let mut times = Vec::with_capacity(iterations);
    for _ in 0..iterations {
      let mut detector = KneeDetector::new();
      detector.add_samples(samples.clone());
      let start = Instant::now();
      let knee = detector.detect_knee();
      let elapsed_us = start.elapsed().as_secs_f64() * 1_000_000.0;
      times.push(elapsed_us);
      assert!(knee.is_some(), "knee must be detected in synthetic series");
    }
    measured.push((name, median(&mut times)));
  }

  // 2. Adaptive policy evaluation
  {
    let name = "adaptive_policy_evaluation";
    let mut detector = KneeDetector::new();
    detector.add_samples(samples.clone());
    let policy =
      AdaptiveContextPolicy::new(ContextProfile::Balanced, 128_000, true).with_detector(&detector);
    let state = ContextState {
      window: 128_000,
      estimated_tokens: 35_000,
      measured_tokens: Some(35_000),
      recent_tokens: 8_000,
      working_messages: 15,
      context_epoch: 0,
      at_safe_boundary: true,
      since_last_compaction_ms: 10_000,
      overflow_observed: false,
    };

    let mut times = Vec::with_capacity(iterations);
    for _ in 0..iterations {
      let start = Instant::now();
      let _decision = policy.evaluate(&state);
      let elapsed_us = start.elapsed().as_secs_f64() * 1_000_000.0;
      times.push(elapsed_us);
    }
    measured.push((name, median(&mut times)));
  }

  // 3. Static vs adaptive comparison
  {
    let name = "static_vs_adaptive_comparison";
    let mut detector = KneeDetector::new();
    detector.add_samples(samples.clone());
    let policy =
      AdaptiveContextPolicy::new(ContextProfile::Balanced, 128_000, true).with_detector(&detector);

    let mut times = Vec::with_capacity(iterations);
    for _ in 0..iterations {
      let start = Instant::now();
      let comp = policy.compare_with_static();
      let elapsed_us = start.elapsed().as_secs_f64() * 1_000_000.0;
      times.push(elapsed_us);
      assert!(comp.adaptive_active);
    }
    measured.push((name, median(&mut times)));
  }

  // 4. Standby trade-off evaluation
  {
    let name = "standby_tradeoff_evaluation";
    let mut times = Vec::with_capacity(iterations);
    for _ in 0..iterations {
      let start = Instant::now();
      let analysis = evaluate_standby_tradeoff(12.5, 1_048_576, 18.0);
      let elapsed_us = start.elapsed().as_secs_f64() * 1_000_000.0;
      times.push(elapsed_us);
      assert!(analysis.startup_penalty_ms > 0.0);
    }
    measured.push((name, median(&mut times)));
  }

  // 5. MCP exposure evaluation
  {
    let name = "mcp_exposure_evaluation";
    let tools = vec![
      McpToolSchemaSummary::new("db", "query_sql", 400, vec!["sql".into()]),
      McpToolSchemaSummary::new("fs", "find_file", 250, vec!["file".into()]),
      McpToolSchemaSummary::new("git", "git_status", 300, vec!["git".into()]),
    ];
    let mut times = Vec::with_capacity(iterations);
    for _ in 0..iterations {
      let start = Instant::now();
      let evals = evaluate_mcp_exposure(&tools, "check git status", 16_000);
      let elapsed_us = start.elapsed().as_secs_f64() * 1_000_000.0;
      times.push(elapsed_us);
      assert_eq!(evals.len(), 3);
    }
    measured.push((name, median(&mut times)));
  }

  let mut breached = Vec::new();
  for (name, us) in &measured {
    let budget = BUDGETS
      .iter()
      .find(|(n, _)| n == name)
      .map(|(_, b)| *b)
      .unwrap_or(f64::INFINITY);
    let status = if *us <= budget { "OK" } else { "BREACH" };
    println!("  {name:<32} {us:>8.2} us (budget: {budget:>8.2} us) [{status}]");
    if *us > budget {
      breached.push((*name, *us, budget));
    }
  }

  if let Some(json_path) = json_out {
    let json_items: Vec<serde_json::Value> = measured
      .iter()
      .map(|(name, us)| {
        serde_json::json!({
          "case": name,
          "median_us": us,
          "iterations": iterations,
        })
      })
      .collect();
    let json_obj = serde_json::json!({
      "benchmark": "context_prefill",
      "iterations": iterations,
      "results": json_items,
      "all_passed": breached.is_empty(),
    });
    if let Some(parent) = std::path::Path::new(&json_path).parent() {
      let _ = fs::create_dir_all(parent);
    }
    if let Err(e) = fs::write(&json_path, serde_json::to_string_pretty(&json_obj).unwrap()) {
      eprintln!("failed to write json output to {json_path}: {e}");
    } else {
      println!("Results written to {json_path}");
    }
  }

  if !breached.is_empty() {
    eprintln!(
      "\nError: {} benchmark cases breached budgets!",
      breached.len()
    );
    ExitCode::FAILURE
  } else {
    ExitCode::SUCCESS
  }
}
