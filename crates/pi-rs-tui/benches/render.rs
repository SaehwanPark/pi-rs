//! Render and command-parse benchmark.
//!
//! Why this exists: rendering is the part of `pi-rs` a user touches on every keystroke and
//! every streamed token, and "startup is fast" says nothing about it. Phase 0 of the roadmap
//! asks for a TUI render benchmark and for initial latency budgets; this is both, and the
//! budgets are enforced here rather than written down and forgotten.
//!
//! The harness is hand-rolled and takes no dependency. A benchmark framework would buy
//! outlier statistics this file does not need, at the cost of a dependency in the same crate
//! whose startup path the project guards: the numbers below are best-of-N medians, which are
//! stable enough to catch a regression and not precise enough to be mistaken for a profile.
//!
//! What is measured is the renderer, not a terminal: each case renders a fixed session and
//! reports microseconds per event. `bench/render.sh` is the entry point, and `--json` writes
//! the same numbers for archival.
//!
//! Run: `cargo bench -p pi-rs-tui --bench render`

use std::env;
use std::fs;
use std::process::ExitCode;
use std::time::Instant;

use pi_rs_core::{
  AgentEvent, AssistantDelta, Diagnostic, DiagnosticLevel, ReasoningDelta, ReasoningProvenance,
  ToolCallId, ToolCompleted, ToolExecutionState, ToolRequested, ToolStarted, UserMessage,
};
use pi_rs_tui::{Input, Palette, TranscriptOptions, render_event};

/// Microseconds per unit of work above which the case is a regression.
///
/// These are the roadmap's initial latency budgets, and the enforcement point: the numbers
/// live here, in the file that fails when they are exceeded, not in a document that stays
/// true after the code gets slow.
///
/// Baseline recorded 2026-09-06 on an AMD Ryzen AI MAX+ 395 (32 cores), rustc 1.98.1,
/// `cargo bench -p pi-rs-tui --bench render` at the default 200 iterations, medians stable to
/// 0.02 us across three runs:
///
/// ```text
/// render_width80_plain   3.15 us/event
/// render_width80_color   3.14 us/event
/// render_width20_color   2.46 us/event
/// parse_command_line     0.16 us/line
/// ```
///
/// Each budget is about five times that median: headroom so a noisier or slower machine does
/// not fail a good commit, and tight enough that making rendering twice as expensive does.
/// Re-measure rather than raising a budget because a number moved.
const BUDGETS: &[(&str, f64)] = &[
  ("render_width80_plain", 16.0),
  ("render_width80_color", 16.0),
  ("render_width20_color", 12.0),
  ("parse_command_line", 1.0),
];

const DEFAULT_ITERATIONS: usize = 200;

fn main() -> ExitCode {
  let args: Vec<String> = env::args().skip(1).collect();
  if args.iter().any(|a| a == "--help" || a == "-h") {
    println!("Usage: render [--iterations <N>] [--json <path>]");
    return ExitCode::SUCCESS;
  }
  let iterations = arg_value(&args, "--iterations")
    .and_then(|v| v.parse::<usize>().ok())
    .filter(|n| *n > 0)
    .unwrap_or(DEFAULT_ITERATIONS);
  let json_out = arg_value(&args, "--json").map(String::from);

  let session = session();
  let wide_plain = options(80, Palette::monochrome());
  let wide_color = options(80, Palette::colored());
  let narrow_color = options(20, Palette::colored());

  let mut measured: Vec<(&str, f64, usize)> = Vec::new();
  // `(name, median us per event, lines produced by one pass)`; the line count is printed so a
  // change that quietly stops rendering half the session cannot look like a speed-up.
  for (name, options) in [
    ("render_width80_plain", &wide_plain),
    ("render_width80_color", &wide_color),
    ("render_width20_color", &narrow_color),
  ] {
    let mut per_event = Vec::with_capacity(iterations);
    let mut lines = 0;
    for _ in 0..iterations {
      let start = Instant::now();
      lines = 0;
      for event in &session {
        lines += render_event(event, options).len();
      }
      per_event.push(start.elapsed().as_secs_f64() * 1_000_000.0 / session.len() as f64);
    }
    measured.push((name, median(&mut per_event), lines));
  }

  let lines_to_parse = command_lines();
  let mut per_line = Vec::with_capacity(iterations);
  let mut spans = 0usize;
  for _ in 0..iterations {
    let start = Instant::now();
    spans = 0;
    // Counting the spans is part of the measurement: a parser that quietly produced no spans
    // would otherwise be timed doing none of the work it claims to do.
    for line in &lines_to_parse {
      spans += Input::parse(line).spans().len();
    }
    per_line.push(start.elapsed().as_secs_f64() * 1_000_000.0 / lines_to_parse.len() as f64);
  }
  measured.push(("parse_command_line", median(&mut per_line), spans));

  println!(
    "TUI render benchmark ({iterations} iterations, {} events)",
    session.len()
  );
  let mut breached = Vec::new();
  for (name, us, lines) in &measured {
    let budget = BUDGETS
      .iter()
      .find(|(case, _)| case == name)
      .map(|(_, budget)| *budget)
      .unwrap_or(f64::MAX);
    let status = if *us > budget {
      breached.push((*name, *us, budget));
      "OVER BUDGET"
    } else {
      "ok"
    };
    let produced = if *lines > 0 {
      let unit = if name.starts_with("render") {
        "lines"
      } else {
        "spans"
      };
      format!("  {unit}/pass {lines}")
    } else {
      String::new()
    };
    println!("  {name:<22} {us:>7.2} us/unit  budget {budget:.0} us  {status}{produced}");
  }

  if let Some(path) = json_out {
    let mut json = String::from("{\n  \"iterations\": ");
    json.push_str(&iterations.to_string());
    json.push_str(",\n  \"cases\": {\n");
    for (index, (name, us, lines)) in measured.iter().enumerate() {
      let budget = BUDGETS
        .iter()
        .find(|(case, _)| case == name)
        .map(|(_, budget)| *budget)
        .unwrap_or(0.0);
      json.push_str(&format!(
        "    \"{name}\": {{\"median_us\": {:.3}, \"budget_us\": {budget:.3}, \"lines_per_pass\": {lines}}}",
        us
      ));
      if index + 1 < measured.len() {
        json.push(',');
      }
      json.push('\n');
    }
    json.push_str("  }\n}\n");
    if let Err(error) = fs::write(path, json) {
      eprintln!("could not write results: {error}");
      return ExitCode::FAILURE;
    }
  }

  if breached.is_empty() {
    ExitCode::SUCCESS
  } else {
    for (name, us, budget) in breached {
      eprintln!("budget exceeded: {name} measured {us:.2} us, budget {budget:.0} us");
    }
    ExitCode::FAILURE
  }
}

/// One typed argument's value, e.g. `--iterations 50`.
fn arg_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
  args
    .iter()
    .position(|a| a == flag)
    .and_then(|i| args.get(i + 1))
    .map(String::as_str)
}

fn median(values: &mut [f64]) -> f64 {
  values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
  values[values.len() / 2]
}

fn options(width: usize, palette: Palette) -> TranscriptOptions {
  TranscriptOptions {
    width,
    palette,
    ..TranscriptOptions::default()
  }
}

/// A session shaped like real output: prose that wraps, reasoning that collapses, tool calls
/// with arguments and results, and a warning nobody asked for.
///
/// The shape matters more than the content. Prose is what gets wrapped and coloured, tool
/// events are what get labelled and aligned, and mixed-case text is what a per-character
/// scanner pays for -- so a benchmark of one short line per event would measure the loop, not
/// the renderer.
fn session() -> Vec<AgentEvent> {
  let mut events = vec![AgentEvent::UserMessage(UserMessage {
    text: "Why does the config loader swallow a malformed entry instead of reporting it? \
            Find the place it happens, then tell me what the least destructive fix is."
      .into(),
    attachments: 0,
  })];
  for turn in 0..6 {
    events.push(AgentEvent::ReasoningDelta(ReasoningDelta {
      text: format!(
        "The loader iterates and uses `let _ =` on the parse, so the error is dropped at turn \
         {turn}; the caller cannot tell a missing file from a broken one."
      ),
      provenance: ReasoningProvenance::Native,
      chunk_index: turn,
    }));
    for chunk in 0..5 {
      events.push(AgentEvent::AssistantDelta(AssistantDelta {
        text: format!(
          "Reading `src/config/loader.rs` ({}), the drop is at line {}: `let _ = Entry::parse` \
           swallows the `ParseError`, so the entry is skipped silently and defaults win.",
          turn + 1,
          chunk * 12 + 40
        ),
        chunk_index: chunk,
      }));
    }
    let call_id = ToolCallId::from_string(format!("call-{turn}"));
    events.push(AgentEvent::ToolRequested(ToolRequested {
      call_id: call_id.clone(),
      name: "read".into(),
      arguments: serde_json::json!({
        "path": format!("src/config/loader{}.rs", turn),
        "offset": chunk_start(turn),
        "limit": 200,
      }),
      read_only: true,
    }));
    events.push(AgentEvent::ToolStarted(ToolStarted {
      call_id: call_id.clone(),
      name: "read".into(),
    }));
    events.push(AgentEvent::ToolCompleted(ToolCompleted {
      call_id,
      name: "read".into(),
      state: ToolExecutionState::Succeeded,
      duration_ms: 12 + turn as u64,
      status: None,
      reduced: turn % 2 == 0,
      blob: None,
      visible_bytes: 1_432 + turn as u64 * 97,
    }));
    events.push(AgentEvent::Diagnostic(Diagnostic {
      level: DiagnosticLevel::Warn,
      message: format!(
        "config entry {} has an unknown key `retry`; ignored",
        turn * 3 + 1
      ),
    }));
  }
  events
}

fn chunk_start(turn: u32) -> u64 {
  turn as u64 * 40 + 1
}

/// Representative input lines: a bare prompt, each command shape, a command with a flag, a
/// path argument, and text that only looks like a command.
fn command_lines() -> Vec<String> {
  [
    "why is the build slow",
    "/model openai-codex/gpt-5.6-luna",
    "/compact --policy balanced",
    "/skills --project",
    "/prompt review auth error-handling",
    "/trace --tools --epoch 2",
    "/add-dir ../vendor/ingress/src",
    "not a command: /model would switch, //model is prose",
    "/x",
    "  /indented leading whitespace is not a command",
  ]
  .iter()
  .map(|line| (*line).to_string())
  .collect()
}
