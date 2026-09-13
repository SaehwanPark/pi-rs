//! The `compat` subcommand: inspect an artifact or package for Pi compatibility.

use pi_rs_compat::{
  compat::{self, CompatReport},
  scan::Discovery,
};

use crate::cli::CompatArgs;

/// Run compatibility inspection on a target artifact or package.
pub fn execute(args: CompatArgs) -> Result<(), String> {
  let cwd = std::env::current_dir().map_err(|error| format!("current directory: {error}"))?;
  let discovery = if args.project {
    Discovery::new(cwd).trusted()
  } else {
    Discovery::new(cwd)
  };

  let report = compat::inspect_target(&args.target, &discovery)?;

  if args.json {
    render_json(&report)?;
  } else {
    render_human(&report);
  }

  Ok(())
}

fn render_human(report: &CompatReport) {
  println!("Compatibility target: {}", report.pi_target);
  println!("Target: {} ({})\n", report.target, report.kind.as_str());

  for surface in &report.surfaces {
    let detail = surface
      .detail
      .as_deref()
      .map(|d| format!(" ({d})"))
      .unwrap_or_default();
    println!("{} {}{}", surface.status.symbol(), surface.name, detail);
  }

  if !report.diagnostics.is_empty() {
    println!("\nDiagnostics:");
    for diag in &report.diagnostics {
      println!("  {}", diag.trim_end_matches('.'));
    }
  }
}

fn render_json(report: &CompatReport) -> Result<(), String> {
  let surfaces_json: Vec<serde_json::Value> = report
    .surfaces
    .iter()
    .map(|s| {
      let mut map = serde_json::Map::new();
      map.insert(
        "name".to_string(),
        serde_json::Value::String(s.name.clone()),
      );
      map.insert(
        "status".to_string(),
        serde_json::Value::String(s.status.as_str().to_string()),
      );
      if let Some(detail) = &s.detail {
        map.insert(
          "detail".to_string(),
          serde_json::Value::String(detail.clone()),
        );
      }
      serde_json::Value::Object(map)
    })
    .collect();

  let diagnostics_json: Vec<serde_json::Value> = report
    .diagnostics
    .iter()
    .map(|d| serde_json::Value::String(d.clone()))
    .collect();

  let report_val = serde_json::json!({
    "target": report.target,
    "kind": report.kind.as_str(),
    "pi_target": report.pi_target,
    "surfaces": surfaces_json,
    "diagnostics": diagnostics_json,
  });

  let json_str = serde_json::to_string_pretty(&report_val)
    .map_err(|error| format!("failed to format JSON compatibility report: {error}"))?;
  println!("{json_str}");
  Ok(())
}
