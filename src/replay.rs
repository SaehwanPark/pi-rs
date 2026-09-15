//! `pi-rs replay`: deterministic inspection of recorded execution.
//!
//! This command is deliberately read-only. It parses an existing trace and its optional
//! semantic session projection, then delegates all state reconstruction to `pi-rs-replay`.
//! No provider, tool, workspace, or store writer is opened.

use std::{
  fs,
  io::{self, Write},
  path::{Path, PathBuf},
};

use pi_rs_core::{EventId, EventSeq, SessionRecord, TraceEntry};
use pi_rs_replay::{
  ContinuationComparison, HistoricalBranchPlan, HistoricalTarget, RedactedTraceExportEntry,
  ReplayFilters, ReplayOptions, ReplayReport, ReplayedEvent, replay_trace_with_session,
};
use serde::Serialize;
use serde_json::Value;

use crate::cli::ReplayArgs;

#[derive(Debug, Clone)]
struct ReplayCorpus {
  trace: Vec<TraceEntry>,
  session: Vec<SessionRecord>,
}

#[derive(Debug, Serialize)]
struct ReplayOutput {
  report: ReplayReport,
  #[serde(skip_serializing_if = "Option::is_none")]
  context: Option<pi_rs_replay::ContextReconstruction>,
  #[serde(skip_serializing_if = "Option::is_none")]
  branch: Option<HistoricalBranchPlan>,
  #[serde(skip_serializing_if = "Option::is_none")]
  comparison: Option<ContinuationComparison>,
  #[serde(skip_serializing_if = "Option::is_none")]
  exported: Option<Vec<RedactedTraceExportEntry>>,
}

pub fn execute(args: ReplayArgs) -> Result<(), String> {
  let corpus = read_corpus(&args.input)?;
  if corpus.trace.is_empty() {
    return Err(format!(
      "replay input '{}' contains no trace events",
      args.input.display()
    ));
  }

  let filters = ReplayFilters {
    tools: args.tools,
    reasoning: args.reasoning,
    timing: args.timing,
  };
  let until = args.until.as_deref().map(parse_target).transpose()?;
  let report = replay_trace_with_session(
    &corpus.trace,
    &corpus.session,
    &ReplayOptions {
      filters,
      until: until.clone(),
    },
  )
  .map_err(|error| error.to_string())?;
  let context_target = args.context_at.as_deref().map(parse_target).transpose()?;
  let context = context_target
    .as_ref()
    .map(|target| context_at(&corpus, target))
    .transpose()?;

  let branch = args.branch.as_deref().map(parse_target).transpose()?;
  let branch = branch
    .as_ref()
    .map(|target| {
      pi_rs_replay::plan_historical_branch(&corpus.trace, &corpus.session, target.clone())
    })
    .transpose()
    .map_err(|error| error.to_string())?;

  let comparison = if let Some(path) = args.compare.as_deref() {
    let other = read_corpus(path)?;
    let base = branch
      .as_ref()
      .map(|plan| plan.branch_point.clone())
      .or_else(|| report.events.last().map(|event| event.reference.clone()))
      .ok_or_else(|| "cannot compare an empty replay base".to_string())?;
    Some(pi_rs_replay::compare_continuations(
      base,
      &corpus.trace,
      &other.trace,
    ))
  } else {
    None
  };

  let exported = args.export.as_ref().map(|path| {
    let selected = selected_entries(&corpus.trace, &report.events);
    let values = pi_rs_replay::export_redacted_trace(&selected, &ReplayFilters::all());
    write_export(path, &values)?;
    Ok::<_, String>(values)
  });
  let exported = exported.transpose()?;

  let output = ReplayOutput {
    report,
    context,
    branch,
    comparison,
    exported,
  };
  if args.json {
    let text = serde_json::to_string_pretty(&output)
      .map_err(|error| format!("cannot encode replay report: {error}"))?;
    println!("{text}");
  } else if output.context.is_some() || output.branch.is_some() || output.comparison.is_some() {
    print_json_fragment(&output)?;
  } else {
    write_human(&output.report, args.sequence, args.timing)?;
  }
  Ok(())
}

fn read_corpus(path: &Path) -> Result<ReplayCorpus, String> {
  let text = fs::read_to_string(path)
    .map_err(|error| format!("cannot read replay input '{}': {error}", path.display()))?;
  let mut trace = Vec::new();
  let mut session = Vec::new();
  for (line_number, line) in text.lines().enumerate() {
    if line.trim().is_empty() {
      continue;
    }
    let value: Value = serde_json::from_str(line).map_err(|error| {
      format!(
        "cannot decode replay input '{}' line {}: {error}",
        path.display(),
        line_number + 1
      )
    })?;
    if value.get("meta").is_some() {
      trace.push(serde_json::from_value(value).map_err(|error| {
        format!(
          "invalid trace entry in '{}' line {}: {error}",
          path.display(),
          line_number + 1
        )
      })?);
    } else if value.get("type").is_some() {
      session.push(serde_json::from_value(value).map_err(|error| {
        format!(
          "invalid session record in '{}' line {}: {error}",
          path.display(),
          line_number + 1
        )
      })?);
    } else {
      return Err(format!(
        "replay input '{}' line {} is neither a trace entry nor a session record",
        path.display(),
        line_number + 1
      ));
    }
  }

  // A normal session state file is paired with `<id>.trace.jsonl`. Loading the sibling keeps
  // `pi-rs replay sessions/<id>.jsonl` useful while still allowing a standalone trace fixture.
  if trace.is_empty() {
    if let Some(sibling) = trace_sibling(path) {
      if sibling.exists() {
        trace = read_trace_only(&sibling)?;
      }
    }
  }
  // Conversely, a trace input can discover the semantic message projection beside it.
  if session.is_empty() {
    if let Some(sibling) = session_sibling(path) {
      if sibling.exists() {
        session = read_session_only(&sibling)?;
      }
    }
  }
  Ok(ReplayCorpus { trace, session })
}

fn read_trace_only(path: &Path) -> Result<Vec<TraceEntry>, String> {
  let corpus = read_corpus_without_siblings(path, true)?;
  Ok(corpus.trace)
}

fn read_session_only(path: &Path) -> Result<Vec<SessionRecord>, String> {
  let corpus = read_corpus_without_siblings(path, false)?;
  Ok(corpus.session)
}

fn read_corpus_without_siblings(path: &Path, trace_only: bool) -> Result<ReplayCorpus, String> {
  let text = fs::read_to_string(path)
    .map_err(|error| format!("cannot read replay input '{}': {error}", path.display()))?;
  let mut trace = Vec::new();
  let mut session = Vec::new();
  for (line_number, line) in text.lines().enumerate() {
    if line.trim().is_empty() {
      continue;
    }
    let value: Value = serde_json::from_str(line).map_err(|error| {
      format!(
        "cannot decode replay input '{}' line {}: {error}",
        path.display(),
        line_number + 1
      )
    })?;
    if value.get("meta").is_some() {
      if !trace_only && !session.is_empty() {
        return Err(format!("mixed replay input '{}'", path.display()));
      }
      trace.push(
        serde_json::from_value(value)
          .map_err(|error| format!("invalid trace entry in '{}': {error}", path.display()))?,
      );
    } else if value.get("type").is_some() {
      if trace_only && !trace.is_empty() {
        return Err(format!("mixed replay input '{}'", path.display()));
      }
      session.push(
        serde_json::from_value(value)
          .map_err(|error| format!("invalid session record in '{}': {error}", path.display()))?,
      );
    }
  }
  Ok(ReplayCorpus { trace, session })
}

fn trace_sibling(path: &Path) -> Option<PathBuf> {
  let name = path.file_name()?.to_str()?;
  let stem = name.strip_suffix(".jsonl")?;
  Some(path.with_file_name(format!("{stem}.trace.jsonl")))
}

fn session_sibling(path: &Path) -> Option<PathBuf> {
  let name = path.file_name()?.to_str()?;
  let stem = name.strip_suffix(".trace.jsonl")?;
  Some(path.with_file_name(format!("{stem}.jsonl")))
}

fn parse_target(value: &str) -> Result<HistoricalTarget, String> {
  if let Some(seq) = value.strip_prefix("seq:") {
    return seq
      .parse::<u64>()
      .map(EventSeq)
      .map(HistoricalTarget::Seq)
      .map_err(|_| format!("invalid replay sequence target '{value}'"));
  }
  if let Some(event) = value.strip_prefix("event:") {
    if event.trim().is_empty() {
      return Err(format!("invalid replay event target '{value}'"));
    }
    return Ok(HistoricalTarget::EventId(EventId::from_string(event)));
  }
  Err(format!(
    "replay target must use event:<id> or seq:<n>, got '{value}'"
  ))
}

fn context_at(
  corpus: &ReplayCorpus,
  target: &HistoricalTarget,
) -> Result<pi_rs_replay::ContextReconstruction, String> {
  let plan = pi_rs_replay::plan_historical_branch(&corpus.trace, &corpus.session, target.clone())
    .map_err(|error| error.to_string())?;
  Ok(plan.context)
}

fn selected_entries(entries: &[TraceEntry], events: &[ReplayedEvent]) -> Vec<TraceEntry> {
  events
    .iter()
    .filter_map(|event| {
      entries
        .iter()
        .find(|entry| entry.envelope.meta.event_id == event.reference.event_id)
        .cloned()
    })
    .collect()
}

fn write_export(path: &Path, entries: &[RedactedTraceExportEntry]) -> Result<(), String> {
  let mut output = String::new();
  for entry in entries {
    output.push_str(
      &serde_json::to_string(entry)
        .map_err(|error| format!("cannot encode replay export: {error}"))?,
    );
    output.push('\n');
  }
  if let Some(parent) = path
    .parent()
    .filter(|parent| !parent.as_os_str().is_empty())
  {
    fs::create_dir_all(parent)
      .map_err(|error| format!("cannot create replay export directory: {error}"))?;
  }
  fs::write(path, output)
    .map_err(|error| format!("cannot write replay export '{}': {error}", path.display()))
}

fn print_json_fragment(output: &ReplayOutput) -> Result<(), String> {
  let text = serde_json::to_string_pretty(output)
    .map_err(|error| format!("cannot encode replay report: {error}"))?;
  println!("{text}");
  Ok(())
}

fn write_human(report: &ReplayReport, sequence: bool, timing: bool) -> Result<(), String> {
  let mut out = io::stdout().lock();
  for event in &report.events {
    let mut line = String::new();
    if sequence {
      if let Some(seq) = event.reference.seq {
        line.push_str(&format!("[{}] ", seq.0));
      }
    }
    line.push_str(
      serde_json::to_value(event.kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .as_deref()
        .unwrap_or("event"),
    );
    match &event.event {
      pi_rs_core::AgentEvent::ReasoningDelta(delta) => {
        line.push_str(&format!(" [{}] {}", delta.provenance.as_str(), delta.text));
      }
      pi_rs_core::AgentEvent::AssistantDelta(delta) => line.push_str(&format!(": {}", delta.text)),
      pi_rs_core::AgentEvent::UserMessage(message) => line.push_str(&format!(": {}", message.text)),
      pi_rs_core::AgentEvent::ToolRequested(tool) => line.push_str(&format!(" {}", tool.name)),
      pi_rs_core::AgentEvent::ToolStarted(tool) => line.push_str(&format!(" {}", tool.name)),
      pi_rs_core::AgentEvent::ToolCompleted(tool) => line.push_str(&format!(" {}", tool.name)),
      pi_rs_core::AgentEvent::ToolFailed(tool) => line.push_str(&format!(" {}", tool.name)),
      pi_rs_core::AgentEvent::ToolUnknown(tool) => line.push_str(&format!(" {}", tool.name)),
      _ => {}
    }
    if timing {
      if let Some(fact) = &event.timing {
        line.push_str(&format!(" @{}ms", fact.timestamp_ms));
        if let Some(duration) = fact.duration_ms {
          line.push_str(&format!(" duration={}ms", duration));
        }
      }
    }
    writeln!(out, "{line}").map_err(|error| format!("cannot write replay output: {error}"))?;
  }
  out
    .flush()
    .map_err(|error| format!("cannot flush replay output: {error}"))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn target_parser_requires_explicit_coordinate() {
    assert_eq!(
      parse_target("seq:4").unwrap(),
      HistoricalTarget::Seq(EventSeq(4))
    );
    assert!(parse_target("4").is_err());
    assert!(parse_target("event:").is_err());
  }

  #[test]
  fn sibling_paths_follow_store_jsonl_names() {
    assert_eq!(
      trace_sibling(Path::new("/tmp/abc.jsonl")).unwrap(),
      PathBuf::from("/tmp/abc.trace.jsonl")
    );
    assert_eq!(
      session_sibling(Path::new("/tmp/abc.trace.jsonl")).unwrap(),
      PathBuf::from("/tmp/abc.jsonl")
    );
  }
}
