//! The recorded view of a session.
//!
//! The live surface ([`crate::Surface`]) renders events as they arrive; this module
//! renders the same events afterwards, from the canonical trace. Both go through
//! [`render_event`], which is why "what I watched while it ran" and "what the session
//! recorded" cannot quietly become two different stories.
//!
//! Two differences from the live view are deliberate:
//!
//! * Fragment events fold. The journal keeps one line per streamed chunk, so one assistant
//!   sentence can occupy forty records, and printing forty lines buries the tool calls a
//!   reader came for. Folding concatenates text exactly — it never rewrites or summarises —
//!   and control characters stay escaped, so folding cannot smuggle an unaddressable line
//!   into a view whose whole purpose is addressing lines by sequence.
//! * Selection is explicit. Narrowing a trace to the tool calls, or to one model epoch, is
//!   a question about the record rather than a preference about chattiness, so it goes
//!   through [`TraceSelection`] and not through [`DiagnosticFilter`][crate::DiagnosticFilter], which decides volume.

use pi_rs_core::{AgentEvent, EventSeq, ReasoningProvenance, TraceEntry};

use crate::line::RenderLine;
use crate::style::Role;
use crate::transcript::{escape_control, label, reasoning_label, render_event, wrap_lines};
use crate::{TranscriptOptions, reasoning_role};

/// Which slice of a recorded session a trace view shows.
///
/// The categories OR together and the epoch ANDs across them, which is what
/// `pi-rs trace --tools --epoch 1` means to a reader: the tool calls made by the model
/// that took over.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TraceSelection {
  /// Tool lifecycle events only: requested, started, and every terminal state.
  pub tools: bool,
  /// Reasoning, plus the epoch events that say which model produced it.
  pub reasoning: bool,
  /// Events attributed to this model epoch only.
  ///
  /// An event with no epoch attribution is not shown. It cannot be shown to belong to the
  /// requested epoch, and presenting an unattributed line as evidence about that epoch
  /// would be a guess dressed as a record. Drop the filter to see the session header and
  /// any unattributed line.
  pub epoch: Option<u32>,
}

impl TraceSelection {
  /// Whether this event belongs to the selected slice.
  ///
  /// `epoch` is the event's own attribution, not the session's current one.
  pub fn shows(&self, event: &AgentEvent, epoch: Option<u32>) -> bool {
    if let Some(wanted) = self.epoch {
      return epoch == Some(wanted);
    }
    if !self.tools && !self.reasoning {
      return true;
    }
    if self.tools && is_tool(event) {
      return true;
    }
    self.reasoning && is_reasoning(event)
  }
}

/// One rendered trace line, carrying the address a reader quotes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedEntry {
  /// Sequence number assigned by the durable log.
  pub seq: EventSeq,
  /// When the event happened, as recorded.
  pub timestamp_ms: u64,
  /// The rendered line.
  pub line: RenderLine,
}

/// Render recorded entries in trace order.
///
/// One event may render several lines, and those lines share a sequence number. A folded
/// fragment run carries the sequence of its first chunk, because that is where the run
/// begins.
pub fn render_trace(
  entries: &[TraceEntry],
  options: &TranscriptOptions,
  selection: &TraceSelection,
) -> Vec<RenderedEntry> {
  let mut rendered: Vec<RenderedEntry> = Vec::new();
  let mut index = 0;
  while index < entries.len() {
    let entry = &entries[index];
    let event = &entry.envelope.event;
    // `prints` is the rule the live surface applies, so a trace and the turn it came from
    // cannot disagree about what `--quiet` means. A folded prose run is routine material
    // under it, which is also why the fold lives here rather than inside `render_event`,
    // where it would hide a live turn's answer as well.
    if !selection.shows(event, entry.envelope.meta.model_epoch)
      || !options.diagnostics.prints(event)
    {
      index += 1;
      continue;
    }
    match event {
      AgentEvent::AssistantDelta(_) => {
        let (next, text) = fold_assistant(entries, index);
        {
          let mut line = label("answer");
          line.push(&escape_control(&text), Role::Assistant);
          emit(&mut rendered, entry, line, options);
        }
        index = next;
      }
      AgentEvent::ReasoningDelta(delta) => {
        let provenance = delta.provenance;
        let (next, text) = fold_reasoning(entries, index, provenance);
        if options.show_reasoning {
          let mut line = RenderLine::new();
          line.push(&reasoning_label(provenance), Role::Muted);
          line.push(&escape_control(&text), reasoning_role(provenance));
          emit(&mut rendered, entry, line, options);
        }
        index = next;
      }
      other => {
        for line in render_event(other, options) {
          emit(&mut rendered, entry, line, options);
        }
        index += 1;
      }
    }
  }
  rendered
}

/// Tool lifecycle events, which is what "show me what it did" selects.
fn is_tool(event: &AgentEvent) -> bool {
  matches!(
    event,
    AgentEvent::ToolRequested(_)
      | AgentEvent::ToolStarted(_)
      | AgentEvent::ToolCompleted(_)
      | AgentEvent::ToolFailed(_)
      | AgentEvent::ToolUnknown(_)
  )
}

/// The reasoning slice.
///
/// Epoch transitions belong here: reasoning is attributable to a model, and an epoch
/// change or failover is the reason a reasoning line's model or provenance changed
/// partway through a turn.
fn is_reasoning(event: &AgentEvent) -> bool {
  matches!(
    event,
    AgentEvent::ReasoningDelta(_) | AgentEvent::ModelEpochStarted(_) | AgentEvent::ModelFailover(_)
  )
}

fn fold_assistant(entries: &[TraceEntry], start: usize) -> (usize, String) {
  let mut index = start;
  let mut text = String::new();
  while let Some(next) = entries.get(index) {
    match &next.envelope.event {
      AgentEvent::AssistantDelta(delta) => {
        text.push_str(&delta.text);
        index += 1;
      }
      _ => break,
    }
  }
  (index, text)
}

/// Concatenate reasoning chunks that share one provenance.
///
/// Provenance is a hard boundary. A run that merged native reasoning with a provider
/// summary would report the combined text under the first chunk's label, which is the
/// provenance loss this crate exists to prevent.
fn fold_reasoning(
  entries: &[TraceEntry],
  start: usize,
  provenance: ReasoningProvenance,
) -> (usize, String) {
  let mut index = start;
  let mut text = String::new();
  while let Some(next) = entries.get(index) {
    match &next.envelope.event {
      AgentEvent::ReasoningDelta(delta) if delta.provenance == provenance => {
        text.push_str(&delta.text);
        index += 1;
      }
      _ => break,
    }
  }
  (index, text)
}

fn emit(
  rendered: &mut Vec<RenderedEntry>,
  entry: &TraceEntry,
  line: RenderLine,
  options: &TranscriptOptions,
) {
  let seq = entry.envelope.meta.seq.unwrap_or(EventSeq(0));
  let timestamp_ms = entry.envelope.meta.timestamp_ms;
  for line in wrap_lines(vec![line], options) {
    rendered.push(RenderedEntry {
      seq,
      timestamp_ms,
      line,
    });
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use pi_rs_core::{
    AssistantDelta, Diagnostic, DiagnosticLevel, EpochReason, EventEnvelope, EventMeta,
    ModelCapabilities, ModelEpochStarted, ModelRef, ReasoningDelta, SessionEndReason, SessionEnded,
    SessionId, ToolCallId, ToolCompleted, ToolExecutionState, ToolFailed, ToolUnknown, TraceId,
  };

  use crate::{DiagnosticFilter, Palette};

  fn entry(seq: u64, epoch: Option<u32>, event: AgentEvent) -> TraceEntry {
    let mut meta = EventMeta::new(SessionId::new(), TraceId::new());
    meta.seq = Some(EventSeq(seq));
    meta.timestamp_ms = seq * 10;
    meta.model_epoch = epoch;
    TraceEntry {
      envelope: EventEnvelope::new(meta, event),
      redactions: 0,
      raw_payload: false,
      raw_ref: None,
    }
  }

  fn info(seq: u64, message: &str) -> TraceEntry {
    entry(
      seq,
      Some(0),
      AgentEvent::Diagnostic(Diagnostic {
        level: DiagnosticLevel::Info,
        message: message.into(),
      }),
    )
  }

  fn reasoning(seq: u64, text: &str, provenance: ReasoningProvenance) -> TraceEntry {
    entry(
      seq,
      Some(0),
      AgentEvent::ReasoningDelta(ReasoningDelta {
        text: text.into(),
        provenance,
        chunk_index: 0,
      }),
    )
  }

  fn assistant(seq: u64, text: &str) -> TraceEntry {
    entry(
      seq,
      Some(0),
      AgentEvent::AssistantDelta(AssistantDelta {
        text: text.into(),
        chunk_index: 0,
      }),
    )
  }

  fn succeeded(seq: u64, epoch: Option<u32>, name: &str) -> TraceEntry {
    entry(
      seq,
      epoch,
      AgentEvent::ToolCompleted(ToolCompleted {
        call_id: ToolCallId::new(),
        name: name.into(),
        state: ToolExecutionState::Succeeded,
        duration_ms: 5,
        status: None,
        reduced: false,
        blob: None,
        visible_bytes: 3,
      }),
    )
  }

  fn failed(seq: u64, name: &str) -> TraceEntry {
    entry(
      seq,
      Some(0),
      AgentEvent::ToolFailed(ToolFailed {
        call_id: ToolCallId::new(),
        name: name.into(),
        message: "exit 1".into(),
        duration_ms: 1,
        status: Some(1),
      }),
    )
  }

  fn rendered(
    entries: &[TraceEntry],
    selection: TraceSelection,
    options: &TranscriptOptions,
  ) -> Vec<String> {
    render_trace(entries, options, &selection)
      .into_iter()
      .map(|entry| entry.line.plain())
      .collect()
  }

  fn shown(
    entries: &[TraceEntry],
    selection: TraceSelection,
    filter: DiagnosticFilter,
  ) -> Vec<String> {
    rendered(
      entries,
      selection,
      &TranscriptOptions {
        diagnostics: filter,
        ..TranscriptOptions::default()
      },
    )
  }

  #[test]
  fn a_fragment_run_folds_to_one_line_at_its_first_chunk() {
    let entries = vec![
      info(1, "context reduced"),
      assistant(2, "The fix "),
      assistant(3, "is in "),
      assistant(4, "width.rs."),
    ];
    let out = render_trace(
      &entries,
      &TranscriptOptions::default(),
      &TraceSelection::default(),
    );
    let addresses: Vec<(u64, String)> = out.iter().map(|r| (r.seq.0, r.line.plain())).collect();
    assert_eq!(
      addresses,
      vec![
        (1, "[info] context reduced".to_string()),
        (2, "[answer] The fix is in width.rs.".to_string()),
      ],
      "{addresses:?}"
    );
    assert_eq!(out[1].timestamp_ms, 20, "the run is dated where it began");
  }

  #[test]
  fn folding_never_invents_a_line_break() {
    // A transcript line stays a line in the recorded view too: an unaddressable
    // continuation would break the sequence addressing this whole view exists for.
    let entries = vec![assistant(1, "one\ntwo\tthree\u{7}")];
    assert_eq!(
      shown(&entries, TraceSelection::default(), DiagnosticFilter::All),
      vec!["[answer] one\\ntwo\\tthree\\u{7}"]
    );
  }

  #[test]
  fn provenance_boundaries_split_a_reasoning_run() {
    let entries = vec![
      reasoning(1, "native half ", ReasoningProvenance::Native),
      reasoning(2, "then a summary", ReasoningProvenance::ProviderSummary),
    ];
    let out = shown(&entries, TraceSelection::default(), DiagnosticFilter::All);
    assert_eq!(out.len(), 2, "{out:?}");
    assert!(out[0].starts_with("[reasoning]"), "{out:?}");
    assert!(out[1].starts_with("[provider summary]"), "{out:?}");
    assert!(
      !out[1].contains("native half"),
      "provenance may not merge: {out:?}"
    );
  }

  #[test]
  fn tools_selects_lifecycle_and_nothing_else() {
    let entries = vec![
      info(1, "context reduced"),
      reasoning(2, "think", ReasoningProvenance::Native),
      assistant(3, "said"),
      failed(4, "exec"),
      succeeded(5, Some(0), "write"),
    ];
    let selection = TraceSelection {
      tools: true,
      ..TraceSelection::default()
    };
    let out = shown(&entries, selection, DiagnosticFilter::All);
    assert_eq!(out.len(), 2, "{out:?}");
    assert!(out[0].contains("[tool failed] exec"), "{out:?}");
    assert!(out[1].contains("[tool ok] write"), "{out:?}");
  }

  #[test]
  fn reasoning_includes_the_epoch_events_that_explain_it() {
    let entries = vec![
      info(1, "context reduced"),
      assistant(2, "said"),
      reasoning(3, "think", ReasoningProvenance::Native),
      entry(
        4,
        Some(1),
        AgentEvent::ModelEpochStarted(ModelEpochStarted {
          epoch: 1,
          model: ModelRef::new("fake", "backup"),
          reason: EpochReason::AutomaticFailover,
          capabilities: ModelCapabilities::text_only(1000),
        }),
      ),
    ];
    let selection = TraceSelection {
      reasoning: true,
      ..TraceSelection::default()
    };
    let out = shown(&entries, selection, DiagnosticFilter::All);
    assert_eq!(out.len(), 2, "{out:?}");
    assert!(out[0].starts_with("[reasoning]"), "{out:?}");
    assert!(out[1].contains("fake/backup"), "{out:?}");
  }

  #[test]
  fn epoch_selection_drops_unattributed_lines() {
    let unattributed = entry(
      1,
      None,
      AgentEvent::SessionEnded(SessionEnded {
        reason: SessionEndReason::UserExit,
      }),
    );
    let entries = vec![
      unattributed,
      succeeded(2, Some(0), "write"),
      succeeded(3, Some(1), "exec"),
    ];
    let selection = TraceSelection {
      epoch: Some(1),
      ..TraceSelection::default()
    };
    let out = shown(&entries, selection, DiagnosticFilter::All);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].contains("[tool ok] exec"), "{out:?}");
  }

  #[test]
  fn quiet_keeps_trouble_and_folds_prose_away() {
    let entries = vec![
      assistant(1, "said"),
      reasoning(2, "think", ReasoningProvenance::Native),
      succeeded(3, Some(0), "write"),
      entry(
        4,
        Some(0),
        AgentEvent::ToolUnknown(ToolUnknown {
          call_id: ToolCallId::new(),
          name: "exec".into(),
          why: "no recorded status".into(),
          mutating: true,
        }),
      ),
    ];
    let out = shown(
      &entries,
      TraceSelection::default(),
      DiagnosticFilter::WarnAndError,
    );
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].contains("[needs check] exec"), "{out:?}");
  }

  #[test]
  fn silent_renders_nothing() {
    let entries = vec![info(1, "context reduced"), succeeded(2, Some(0), "write")];
    assert_eq!(
      shown(&entries, TraceSelection::default(), DiagnosticFilter::None),
      Vec::<String>::new()
    );
  }

  #[test]
  fn hidden_reasoning_does_not_swallow_the_next_line() {
    let entries = vec![
      reasoning(1, "think", ReasoningProvenance::Native),
      succeeded(2, Some(0), "write"),
    ];
    let out = rendered(
      &entries,
      TraceSelection::default(),
      &TranscriptOptions {
        show_reasoning: false,
        ..TranscriptOptions::default()
      },
    );
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].contains("[tool ok] write"), "{out:?}");
  }

  #[test]
  fn a_labelled_line_wraps_the_same_whether_live_or_recorded() {
    // Same renderer, same width: a folded answer must not wrap differently from the
    // tool line the live surface would have printed at the same budget.
    let tool = render_event(
      &failed(1, "exec").envelope.event,
      &TranscriptOptions {
        width: 20,
        ..TranscriptOptions::default()
      },
    );
    let answer = render_trace(
      &[assistant(1, "one two three four")],
      &TranscriptOptions {
        width: 20,
        ..TranscriptOptions::default()
      },
      &TraceSelection::default(),
    );
    let tool_lines: Vec<String> = tool.iter().map(|l| l.plain()).collect();
    let answer_lines: Vec<String> = answer.iter().map(|l| l.line.plain()).collect();
    for line in tool_lines.iter().chain(answer_lines.iter()) {
      assert!(crate::display_width(line) <= 20, "{line:?}");
    }
    eprintln!("TOOL {tool_lines:?}");
    assert!(tool_lines[0].starts_with("[tool failed]"), "{tool_lines:?}");
    assert!(answer_lines[0].starts_with("[answer]"), "{answer_lines:?}");
  }

  #[test]
  fn a_folded_run_wraps_like_any_other_line_and_keeps_one_address() {
    let entries = vec![assistant(1, "one two three four five six seven")];
    let out = render_trace(
      &entries,
      &TranscriptOptions {
        width: 20,
        ..TranscriptOptions::default()
      },
      &TraceSelection::default(),
    );
    let texts: Vec<String> = out.iter().map(|r| r.line.plain()).collect();
    eprintln!("{texts:?}");
    assert_eq!(
      texts,
      vec!["[answer] one two", "three four five six", "seven"],
      "{texts:?}"
    );
    assert!(
      out.iter().all(|r| r.seq == EventSeq(1)),
      "one run, one address"
    );
  }

  #[test]
  fn colour_is_a_projection_over_the_same_words() {
    let entries = vec![failed(1, "exec"), assistant(2, "said")];
    let coloured = TranscriptOptions {
      palette: Palette::colored(),
      ..TranscriptOptions::default()
    };
    let plain_options = TranscriptOptions::default();
    let with_colour = render_trace(&entries, &coloured, &TraceSelection::default());
    let without = render_trace(&entries, &plain_options, &TraceSelection::default());
    assert_eq!(with_colour.len(), without.len());
    for (colour, plain) in with_colour.iter().zip(&without) {
      assert_eq!(colour.line.plain(), plain.line.plain());
    }
  }
}
