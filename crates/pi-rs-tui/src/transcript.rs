//! Deterministic event-to-line rendering.
//!
//! One question per function: given one canonical event, what lines does the
//! surface show? This module holds no state and performs no I/O, which is what
//! makes it testable and what lets the live transcript, `pi-rs trace`, and any
//! later export share one answer instead of three similar ones.
//!
//! Three rendering rules are load-bearing rather than stylistic:
//!
//! 1. **Reasoning always carries its provenance label.** Unlabelled
//!    thinking-shaped text is indistinguishable from prose, and "the model emitted
//!    this" versus "we inferred this afterwards" is exactly the distinction AGENTS
//!    forbids collapsing.
//! 2. **`unknown` is never spelled `failed`.** It gets its own prefix, its own
//!    role, and, when the call could have mutated state, its own warning.
//! 3. **Calm is the default.** Requests, deltas and completions happen thousands
//!    of times; retries, failovers and unknown completions do not. Only the rare
//!    ones are allowed to shout.

use pi_rs_core::{
  AgentEvent, DiagnosticLevel, ModelCapabilities, ReasoningProvenance, SessionEndReason,
  ToolExecutionState, TurnStatus,
};

use crate::{
  command::is_path_shape,
  format::{SEPARATOR, format_bytes, format_duration_ms, format_tokens},
  line::{NarrowDecoration, RenderLine},
  style::{Palette, Role},
  width::{MIN_COLUMN, truncate},
};

/// How much of the surface the caller wants.
///
/// Defaults follow the AGENTS TUI rules: reasoning is shown but never styled as
/// prose, verbose tool output is collapsed, diagnostics are on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptOptions {
  /// Column budget. `0` means "do not wrap", which is what a pipe wants.
  pub width: usize,
  /// Emit ANSI or not.
  pub palette: Palette,
  /// Render reasoning blocks at all.
  pub show_reasoning: bool,
  /// Report a reasoning body's size instead of its text.
  ///
  /// This is the *retrospective* collapse used when rendering a recorded event. A
  /// live stream cannot retroactively hide text it already wrote, so the live
  /// surface makes the same choice up front instead.
  pub collapse_reasoning: bool,
  /// Truncate tool output bodies to the column budget.
  pub collapse_tool_output: bool,
  /// Which diagnostics to render.
  pub diagnostics: DiagnosticFilter,
}

impl Default for TranscriptOptions {
  fn default() -> Self {
    Self {
      width: 0,
      palette: Palette::monochrome(),
      show_reasoning: true,
      collapse_reasoning: false,
      collapse_tool_output: true,
      diagnostics: DiagnosticFilter::All,
    }
  }
}

impl TranscriptOptions {
  /// The column budget, if any.
  pub fn content_width(&self) -> Option<usize> {
    (self.width > 0).then_some(self.width)
  }
}

/// Diagnostic verbosity filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiagnosticFilter {
  /// Everything the renderer can say. Right for `trace` and `replay`, where the
  /// transcript *is* the output.
  #[default]
  All,
  /// Routine chrome is withheld: a running turn should surface warnings, errors, and
  /// state changes, not a narration of every request, model selection, and policy
  /// snapshot. This is what `pi-rs run` prints by default.
  State,
  /// Only bad news: warnings, errors, and tool calls that failed, were refused, or never
  /// recorded a completion. A successful tool call is routine work and has no place in a
  /// log someone is scanning for trouble.
  WarnAndError,
  /// No transcript at all. Never applies to assistant prose, which is the answer rather
  /// than a narration of the turn, and never applies to what the session records.
  None,
}

impl DiagnosticFilter {
  /// Whether a diagnostic of this severity is shown.
  pub fn allows(self, level: DiagnosticLevel) -> bool {
    match self {
      Self::All => true,
      Self::State | Self::WarnAndError => {
        matches!(level, DiagnosticLevel::Warn | DiagnosticLevel::Error)
      }
      Self::None => false,
    }
  }

  /// Whether routine chrome — one line per model request, per config snapshot — may
  /// be printed at all.
  ///
  /// Kept separate from [`Self::shows`] because routine live chrome is decided by the
  /// surface that prints it, while `shows` decides what the durable transcript
  /// contains. Both answer "is this news", from different sides of the same rule.
  pub fn shows_routine(self) -> bool {
    matches!(self, Self::All)
  }

  /// Whether this event earns a transcript line at this level.
  ///
  /// The boundary is *routine versus state-changing*, not *unimportant versus
  /// important*: `[request] #3` and `[model] gpt-5.4` describe machinery working as
  /// configured, and a turn that prints them prints forty lines to say that nothing
  /// happened. A budget reduction, a retry, or a failover is news, and news is what
  /// survives a calm transcript.
  ///
  /// Severity-gating and event-gating are deliberately one question here. Split
  /// them and a renderer filters by severity, then forgets that a session summary is
  /// not a diagnostic.
  /// Whether this event is printed at this level, in either view of a session.
  ///
  /// One rule for two views on purpose: the live surface and the recorded trace must
  /// agree line for line, or `--quiet` would mean one thing while a turn runs and
  /// another afterwards. An event that streamed is judged as routine or news, because
  /// "it arrived and succeeded" is routine while "it failed, was refused, or never
  /// recorded a completion" is not; anything else is judged by category.
  pub fn prints(self, event: &AgentEvent) -> bool {
    if crate::live::is_streamed(event) {
      return match self {
        Self::None => false,
        Self::All | Self::State => true,
        Self::WarnAndError => !crate::live::routine_stream(event),
      };
    }
    self.shows(event)
  }

  pub fn shows(self, event: &AgentEvent) -> bool {
    use AgentEvent as E;
    match event {
      E::Diagnostic(e) => self.allows(e.level),
      _ => match self {
        Self::All => true,
        Self::None | Self::WarnAndError => false,
        Self::State => matches!(
          event,
          E::SessionEnded(_)
            | E::ContextReduced(_)
            | E::ModelRetry(_)
            | E::ModelFailover(_)
            | E::ModelEpochStarted(_)
            | E::ContextCompactionCompleted(_)
        ),
      },
    }
  }
}

/// The runtime's provenance enum, resolved to a role.
///
/// The mapping exists in exactly one place so that no renderer can reuse one role
/// for two provenances and make a summary look like emitted reasoning.
pub const fn reasoning_role(provenance: ReasoningProvenance) -> Role {
  match provenance {
    ReasoningProvenance::Native => Role::ReasoningNative,
    ReasoningProvenance::ProviderSummary => Role::ReasoningProviderSummary,
    ReasoningProvenance::Declared => Role::ReasoningDeclared,
    ReasoningProvenance::Reconstructed => Role::ReasoningReconstructed,
  }
}

/// `[label] ` prefix a reasoning block is written under.
pub fn reasoning_label(provenance: ReasoningProvenance) -> String {
  format!("[{}] ", provenance.label())
}

/// Render one canonical event to lines.
///
/// Every variant produces at least one line, including ones that carry little
/// content: "the renderer had nothing to say" and "the runtime said nothing" must
/// not look the same in a transcript.
pub fn render_event(event: &AgentEvent, options: &TranscriptOptions) -> Vec<RenderLine> {
  use AgentEvent as E;
  let lines = match event {
    E::SessionStarted(e) => {
      let mut line = label("session");
      line.push(&e.model.as_key(), Role::Operation);
      // The working directory belongs in the header: a session transcript is
      // routinely read later, from somewhere else, and "which checkout?" should not
      // require opening the session file.
      fact(&mut line, Role::Path, &e.working_dir);
      capability_facts(&mut line, &e.capabilities);
      if e.resumed {
        fact(&mut line, Role::Meta, "resumed");
      }
      vec![line]
    }
    E::UserMessage(e) => {
      let mut line = RenderLine::new();
      line.push("> ", Role::Prompt);
      line.push(&e.text, Role::UserText);
      if e.attachments > 0 {
        fact(
          &mut line,
          Role::Meta,
          &format!("{} attachment(s)", e.attachments),
        );
      }
      vec![line]
    }
    E::ModelRequestStarted(e) => {
      let mut line = label("request");
      fact(&mut line, Role::Meta, &format!("epoch {}", e.epoch));
      line.push(SEPARATOR, Role::Muted);
      line.push(&e.model.as_key(), Role::Operation);
      fact(&mut line, Role::Meta, &format!("{} msg", e.message_count));
      fact(
        &mut line,
        Role::Meta,
        &format!("~{} tok est", format_tokens(e.context_tokens_est)),
      );
      fact(&mut line, Role::Meta, &format!("{} tools", e.tools_exposed));
      vec![line]
    }
    E::ReasoningDelta(e) if options.show_reasoning => {
      let role = reasoning_role(e.provenance);
      let mut line = RenderLine::new();
      line.push(&reasoning_label(e.provenance), Role::Muted);
      if options.collapse_reasoning {
        // Report the size of what was withheld. A collapsed block that leaves no
        // trace is how a reader concludes the model never reasoned at all.
        line.push(
          &format!(
            "collapsed ({} chars, chunk {})",
            e.text.chars().count(),
            e.chunk_index
          ),
          role,
        );
      } else {
        line.push(&e.text, role);
      }
      vec![line]
    }
    E::AssistantDelta(e) => vec![RenderLine::text(e.text.clone(), Role::Assistant)],
    E::ModelRequestCompleted(e) => {
      let mut line = label("model");
      line.push(
        e.finish_reason.as_deref().unwrap_or("no finish reason"),
        Role::Meta,
      );
      if let Some(tokens) = e.input_tokens {
        fact(&mut line, Role::Meta, &format!("in {tokens}"));
      }
      if let Some(tokens) = e.output_tokens {
        fact(&mut line, Role::Meta, &format!("out {tokens}"));
      }
      fact(&mut line, Role::Meta, &format_duration_ms(e.duration_ms));
      if e.tool_calls > 0 {
        fact(
          &mut line,
          Role::Meta,
          &format!("{} tool call(s)", e.tool_calls),
        );
      }
      if let Some(provenance) = e.reasoning_provenance {
        // Provenance survives coalescing precisely because the completion event,
        // not only the deltas, states it.
        fact(&mut line, Role::Meta, &format!("reasoning: {provenance}"));
        if provenance.is_inferred() {
          fact(
            &mut line,
            Role::ReasoningReconstructed,
            "inference, not emitted thought",
          );
        }
      }
      vec![line]
    }
    E::ModelRetry(e) => {
      let mut line = label("retry");
      line.push(&format!("{} of {}", e.attempt, e.max_attempts), Role::Rare);
      line.push(SEPARATOR, Role::Muted);
      line.push(e.kind.as_str(), Role::Warning);
      if let Some(after) = e.retry_after_ms {
        fact(
          &mut line,
          Role::Meta,
          &format!("in {}", format_duration_ms(after)),
        );
      }
      if e.will_failover {
        fact(&mut line, Role::Rare, "next: backup model");
      }
      vec![line]
    }
    E::ModelFailover(e) => {
      let mut line = label("failover");
      line.push(&format!("{} → {}", e.from, e.to), Role::Rare);
      line.push(SEPARATOR, Role::Muted);
      line.push(e.kind.as_str(), Role::Warning);
      if !e.gaps.is_empty() {
        let gaps: Vec<String> = e.gaps.iter().map(|gap| gap.to_string()).collect();
        fact(&mut line, Role::StateUnknown, &gaps.join(", "));
      }
      if e.compacted {
        fact(&mut line, Role::Meta, "context rebudgeted");
      }
      vec![line]
    }
    E::ModelEpochStarted(e) => {
      let mut line = label("epoch");
      fact(&mut line, Role::Meta, &e.epoch.to_string());
      line.push(SEPARATOR, Role::Muted);
      line.push(&e.model.as_key(), Role::Operation);
      fact(&mut line, Role::Meta, epoch_reason(&e.reason));
      capability_facts(&mut line, &e.capabilities);
      vec![line]
    }
    E::ToolRequested(e) => vec![tool_request_line(&e.name, &e.arguments, e.read_only)],
    E::ToolStarted(e) => {
      let mut line = label("running");
      line.push(&e.name, Role::Operation);
      vec![line]
    }
    E::ToolCompleted(e) => {
      let mut line = label("tool ok");
      line.push(&e.name, Role::Operation);
      line.push(SEPARATOR, Role::Muted);
      line.push(e.state.as_str(), Role::StateOk);
      fact(&mut line, Role::Meta, &format_duration_ms(e.duration_ms));
      if let Some(status) = e.status {
        fact(&mut line, Role::Meta, &format!("exit {status}"));
      }
      fact(
        &mut line,
        Role::Meta,
        &format!("{} visible", format_bytes(e.visible_bytes)),
      );
      if e.reduced {
        fact(&mut line, Role::Warning, "reduced");
        if let Some(blob) = &e.blob {
          fact(&mut line, Role::Path, &blob.recovery_ref());
        }
      }
      vec![line]
    }
    E::ToolFailed(e) => {
      let mut line = label("tool failed");
      line.push(&e.name, Role::Operation);
      line.push(SEPARATOR, Role::Muted);
      line.push(&e.message, Role::Error);
      fact(&mut line, Role::Meta, &format_duration_ms(e.duration_ms));
      if let Some(status) = e.status {
        fact(&mut line, Role::Meta, &format!("exit {status}"));
      }
      vec![line]
    }
    E::ToolUnknown(e) => {
      // This line is the reason `Unknown` is a state rather than a boolean.
      let mut line = label(completion_label(ToolExecutionState::Unknown, e.mutating));
      line.push(&e.name, Role::Operation);
      line.push(SEPARATOR, Role::Muted);
      line.push(&e.why, Role::StateUnknown);
      if e.mutating {
        fact(
          &mut line,
          Role::Rare,
          "may have changed state; not replayed",
        );
      }
      vec![line]
    }
    E::ExternalContextRetrieved(e) => {
      let mut line = label("external");
      line.push(&e.source.resource_id, Role::Path);
      fact(&mut line, Role::Meta, &e.source.provider);
      fact(&mut line, Role::Meta, &e.source.provenance);
      if let Some(citation) = &e.citation {
        fact(&mut line, Role::Meta, citation);
      }
      fact(&mut line, Role::Meta, &format_bytes(e.bytes));
      fact(
        &mut line,
        Role::Meta,
        if e.inline { "inline" } else { "reference" },
      );
      vec![line]
    }
    E::ContextReduced(e) => {
      let mut line = label("reduced");
      fact(&mut line, Role::Meta, &reduction_reason(&e.reason));
      fact(
        &mut line,
        Role::Meta,
        &format!(
          "{} → {}",
          format_bytes(e.original_bytes),
          format_bytes(e.visible_bytes)
        ),
      );
      match e.recovery_ref.as_deref() {
        Some(reference) => fact(&mut line, Role::Path, &format!("recover {reference}")),
        // The withheld bytes are gone. That belongs on the line: a record which only
        // mentioned the size change would make a lost payload look like a harmless
        // summary.
        None => fact(&mut line, Role::StateUnknown, "not recoverable"),
      };
      vec![line]
    }
    E::ContextCompactionStarted(e) => {
      let mut line = label("compact");
      line.push(SEPARATOR, Role::Muted);
      line.push(e.level.as_str(), Role::Meta);
      fact(&mut line, Role::Muted, &e.reason);
      vec![line]
    }
    E::ContextCompactionCompleted(e) => {
      let mut line = label("compact");
      line.push(SEPARATOR, Role::Muted);
      line.push(e.level.as_str(), Role::Meta);
      fact(
        &mut line,
        Role::Meta,
        &format!(
          "retained {} removed {}",
          e.retained_messages, e.removed_messages
        ),
      );
      fact(
        &mut line,
        Role::Meta,
        &format!("context epoch {}", e.context_epoch),
      );
      vec![line]
    }
    E::CheckpointCreated(e) => {
      let mut line = label("checkpoint");
      fact(&mut line, Role::Meta, e.checkpoint_id.as_str());
      fact(
        &mut line,
        Role::Meta,
        &format!("{} events", e.summarized_events),
      );
      fact(&mut line, Role::Path, &e.path);
      vec![line]
    }
    E::TurnCompleted(e) => {
      let mut line = label("turn");
      let (status, role) = match &e.status {
        TurnStatus::Completed => ("completed", Role::StateOk),
        TurnStatus::Cancelled => ("cancelled", Role::Muted),
        TurnStatus::Failed { kind } => (kind.as_str(), Role::StateFailed),
      };
      line.push(status, role);
      fact(&mut line, Role::Meta, &format_duration_ms(e.duration_ms));
      vec![line]
    }
    E::Diagnostic(e) if options.diagnostics.shows(event) => {
      let (level_label, role) = match e.level {
        DiagnosticLevel::Info => ("info", Role::Info),
        DiagnosticLevel::Warn => ("warn", Role::Warning),
        DiagnosticLevel::Error => ("error", Role::Error),
      };
      let mut line = label(level_label);
      line.push(&e.message, role);
      vec![line]
    }
    E::SessionEnded(e) => {
      let mut line = label("session end");
      match &e.reason {
        SessionEndReason::UserExit => {
          line.push("user exit", Role::Muted);
        }
        SessionEndReason::Restart => {
          line.push("restart", Role::Muted);
        }
        SessionEndReason::Fatal { message } => {
          line.push("fatal", Role::Error);
          line.push(SEPARATOR, Role::Muted);
          line.push(message, Role::Error);
        }
      };
      vec![line]
    }
    // Hidden by configuration, not absent: the caller asked for less output.
    E::ReasoningDelta(_) | E::Diagnostic(_) => Vec::new(),
  };
  wrap_lines(lines, options)
}

/// A `[label] `-prefixed line: the shape almost every transcript line starts from.
///
/// Public because the live surface must open a tool line with exactly the label the
/// recorded event would use. Two label sites drift, and the drift shows up as a
/// live transcript that cannot be matched against its own session file.
pub fn label(label: &str) -> RenderLine {
  let mut line = RenderLine::new();
  line.push(&format!("[{label}] "), Role::Muted);
  line
}

/// The `[tool]` request line, shared by the live and recorded paths.
///
/// Argument segmentation is the part worth sharing: an operation, its flag names,
/// and its paths must be separate segments in a live transcript and in a replay of
/// the same session, or the two views of one turn disagree about what was asked.
pub fn tool_request_line(name: &str, arguments: &serde_json::Value, read_only: bool) -> RenderLine {
  let mut line = label("tool");
  line.push(name, Role::Operation);
  push_arguments(&mut line, arguments);
  if read_only {
    fact(&mut line, Role::Meta, "read-only");
  } else {
    // A call that can change state is not yet a change: `mutating` says which kind
    // of risk it carries, and the completion line says whether it landed.
    fact(&mut line, Role::StateUnknown, "mutating");
  }
  line
}

/// The label for a tool state, so a live line and the recorded line for the same
/// outcome cannot name it differently.
///
/// `mutating` matters only for `unknown`, the one state where the runtime genuinely
/// does not know and the user genuinely needs to.
pub fn completion_label(state: ToolExecutionState, mutating: bool) -> &'static str {
  use ToolExecutionState as S;
  match state {
    S::Succeeded => "tool ok",
    S::Failed => "tool failed",
    S::Unknown if mutating => "needs check",
    S::Unknown => "unknown",
    S::Requested => "tool",
    S::Started => "running",
  }
}

/// Append a fact to a meta line.
///
/// The first fact after the `[label] ` prefix is separated by the prefix's own
/// space; later facts take ` · `. Emitting the separator unconditionally is how a
/// line reads `[reduced]  · first fact`, with the ghost gap of a delimiter that had
/// nothing to delimit.
fn fact(line: &mut RenderLine, role: Role, text: &str) {
  if line.segments.len() > 1 {
    line.push(SEPARATOR, Role::Muted);
  }
  line.push(text, role);
}

fn capability_facts(line: &mut RenderLine, capabilities: &ModelCapabilities) {
  fact(
    line,
    Role::Meta,
    &format!("ctx {}", format_tokens(capabilities.context_window)),
  );
  let mut inputs = Vec::new();
  if capabilities.text {
    inputs.push("text");
  }
  if capabilities.images {
    inputs.push("images");
  }
  if capabilities.tools {
    inputs.push("tools");
  }
  fact(line, Role::Meta, &inputs.join("+"));
  use pi_rs_core::ReasoningExposure as Exposure;
  match capabilities.exposed_reasoning {
    Exposure::None => {}
    Exposure::Native => fact(line, Role::ReasoningNative, "reasoning: native"),
    Exposure::ProviderSummary => fact(
      line,
      Role::ReasoningProviderSummary,
      "reasoning: provider summary",
    ),
    Exposure::Declared => fact(line, Role::ReasoningDeclared, "reasoning: declared"),
  }
}

/// How much of one tool argument is worth a line of transcript.
///
/// Beyond roughly a scannable value, an argument is data rather than a signal: a
/// `write` call's `contents` is the file, and printing it would bury the one fact
/// the request line exists to convey — which operation, on which path. The data is
/// in the session file, and the tool's own output line follows.
const MAX_ARGUMENT_COLUMNS: usize = 60;

fn push_arguments(line: &mut RenderLine, arguments: &serde_json::Value) {
  match arguments {
    serde_json::Value::Object(map) => {
      for (key, value) in map {
        line.push(" ", Role::Muted);
        line.push(&format!("{key}="), Role::Meta);
        let text = display_argument(&argument_text(value));
        let role = if value.is_string() && is_path_shape(&text) {
          Role::Path
        } else {
          Role::Argument
        };
        line.push(&text, role);
      }
    }
    other => {
      let text = display_argument(&argument_text(other));
      line.push(" ", Role::Muted);
      line.push(&text, Role::Argument);
    }
  }
}

/// Make one argument value safe to place on a transcript line.
///
/// Control characters are escaped rather than passed through. A newline inside a
/// tool argument would otherwise emit an unlabelled second line, and an unlabelled
/// line in this transcript reads like assistant prose — the worst possible confusion
/// between "the model said this" and "the model asked for this".
fn display_argument(value: &str) -> String {
  let escaped = escape_control(value);
  truncate(&escaped, MAX_ARGUMENT_COLUMNS)
}

/// Escape ASCII control characters as `\\n`, `\\r`, `\\t`, or `\\u{..}`.
///
/// Only control characters are touched: escaping backslashes or quotes too would
/// rewrite paths so that a transcript no longer shows the path that was used.
pub(crate) fn escape_control(value: &str) -> std::borrow::Cow<'_, str> {
  use std::borrow::Cow;
  if !value.chars().any(|c| c.is_ascii_control()) {
    return Cow::Borrowed(value);
  }
  let mut out = String::with_capacity(value.len() + 8);
  for c in value.chars() {
    match c {
      '\n' => out.push_str("\\n"),
      '\r' => out.push_str("\\r"),
      '\t' => out.push_str("\\t"),
      other if other.is_ascii_control() => {
        out.push_str(&format!("\\u{{{:x}}}", other as u32));
      }
      other => out.push(other),
    }
  }
  Cow::Owned(out)
}

fn argument_text(value: &serde_json::Value) -> String {
  match value {
    serde_json::Value::String(text) => text.clone(),
    other => other.to_string(),
  }
}

fn epoch_reason(reason: &pi_rs_core::EpochReason) -> &'static str {
  use pi_rs_core::EpochReason as R;
  match reason {
    R::Initial => "initial",
    R::ManualSwitch => "manual switch",
    R::AutomaticFailover => "automatic failover",
    R::ManualSwitchBack => "manual switch back",
  }
}

fn reduction_reason(reason: &pi_rs_core::ReductionReason) -> String {
  use pi_rs_core::ReductionReason as R;
  match reason {
    R::OversizedToolOutput { limit_bytes } => {
      format!("tool output over {}", format_bytes(*limit_bytes))
    }
    R::RecentTargetExceeded { target_tokens } => {
      format!("recent window over {} tok", format_tokens(*target_tokens))
    }
    R::ExternalContextTooLarge { limit_bytes } => {
      format!("external context over {}", format_bytes(*limit_bytes))
    }
    R::UserRequested => "user requested".to_string(),
  }
}

pub(crate) fn wrap_lines(
  mut lines: Vec<RenderLine>,
  options: &TranscriptOptions,
) -> Vec<RenderLine> {
  if options.width == 0 {
    return lines;
  }
  let decoration = if options.width >= MIN_COLUMN {
    NarrowDecoration::Keep
  } else {
    NarrowDecoration::Strip
  };
  let mut out = Vec::with_capacity(lines.len());
  for line in lines.drain(..) {
    if line.width() <= options.width {
      out.push(line);
    } else {
      out.extend(line.wrapped(options.width, decoration));
    }
  }
  out
}

#[cfg(test)]
mod tests {
  fn request(name: &str, arguments: serde_json::Value) -> AgentEvent {
    AgentEvent::ToolRequested(ToolRequested {
      call_id: ToolCallId::new(),
      name: name.into(),
      arguments,
      read_only: false,
    })
  }

  #[test]
  fn a_control_character_in_an_argument_never_starts_an_unlabelled_line() {
    let event = request(
      "write",
      serde_json::json!({ "path": "a.txt", "contents": "one\ntwo" }),
    );
    let lines = render_event(&event, &options());
    assert_eq!(lines.len(), 1, "{lines:?}");
    let text = lines[0].plain();
    assert!(text.contains(r"contents=one\ntwo"), "{text}");
    assert!(!text.contains('\n'), "{text:?}");
  }

  #[test]
  fn a_long_argument_value_is_collapsed_not_dumped() {
    let event = request("write", serde_json::json!({ "contents": "x".repeat(4000) }));
    let text = plain(&render_event(&event, &options()));
    // Collapsed, and marked as collapsed: the reader must be able to tell a hidden
    // value from a short one.
    assert!(text.contains('…'), "{text}");
    assert!(text.contains("mutating"), "{text}");
    assert!(text.chars().count() < 120, "{text}");
  }

  #[test]
  fn escaping_leaves_paths_alone() {
    let event = request("read", serde_json::json!({ "path": r"src\main.rs" }));
    assert!(plain(&render_event(&event, &options())).contains(r"path=src\main.rs"));
  }

  use pi_rs_core::{
    AgentEvent, CapabilityGap, CheckpointId, EpochReason, ModelCapabilities, ModelEpochStarted,
    ModelFailover, ModelFailureKind, ModelRef, ReasoningDelta, ReasoningExposure, ReductionReason,
    SessionStarted, ToolCallId, ToolCompleted, ToolRequested, ToolUnknown, trace::BlobRef,
  };

  use super::*;
  use crate::style::Palette;

  fn options() -> TranscriptOptions {
    TranscriptOptions::default()
  }

  fn model() -> ModelRef {
    ModelRef::new("local", "qwen")
  }

  fn plain(lines: &[RenderLine]) -> String {
    lines
      .iter()
      .map(|line| line.plain())
      .collect::<Vec<_>>()
      .join("\n")
  }

  fn reasoning(provenance: ReasoningProvenance) -> AgentEvent {
    AgentEvent::ReasoningDelta(ReasoningDelta {
      text: "because".into(),
      provenance,
      chunk_index: 0,
    })
  }

  #[test]
  fn reasoning_renders_with_four_distinct_labels() {
    let labels: Vec<String> = [
      ReasoningProvenance::Native,
      ReasoningProvenance::ProviderSummary,
      ReasoningProvenance::Declared,
      ReasoningProvenance::Reconstructed,
    ]
    .into_iter()
    .map(|p| plain(&render_event(&reasoning(p), &options())))
    .collect();
    assert_eq!(
      labels,
      vec![
        "[reasoning] because",
        "[provider summary] because",
        "[declared rationale] because",
        "[reconstructed rationale] because"
      ]
    );
  }

  #[test]
  fn reconstructed_never_borrows_the_native_role() {
    let lines = render_event(&reasoning(ReasoningProvenance::Reconstructed), &options());
    assert!(lines[0].plain().starts_with("[reconstructed rationale] "));
    assert!(
      lines[0]
        .segments
        .iter()
        .any(|s| s.role == Role::ReasoningReconstructed)
    );
    assert!(
      !lines[0]
        .segments
        .iter()
        .any(|s| s.role == Role::ReasoningNative),
      "{:?}",
      lines[0].segments
    );
  }

  #[test]
  fn hidden_reasoning_renders_nothing_instead_of_nothing_loudly() {
    let options = TranscriptOptions {
      show_reasoning: false,
      ..options()
    };
    assert!(render_event(&reasoning(ReasoningProvenance::Native), &options).is_empty());
  }

  #[test]
  fn collapsed_reasoning_reports_what_it_withheld() {
    let options = TranscriptOptions {
      collapse_reasoning: true,
      ..options()
    };
    let text = plain(&render_event(
      &AgentEvent::ReasoningDelta(ReasoningDelta {
        text: "a fairly long thought".into(),
        provenance: ReasoningProvenance::ProviderSummary,
        chunk_index: 1,
      }),
      &options,
    ));
    assert!(text.starts_with("[provider summary] collapsed"), "{text}");
    assert!(text.contains("21 chars"), "{text}");
    assert!(!text.contains("fairly long"), "{text}");
  }

  #[test]
  fn unknown_mutating_completion_is_not_rendered_as_failure() {
    let event = AgentEvent::ToolUnknown(ToolUnknown {
      call_id: ToolCallId::new(),
      name: "exec".into(),
      why: "connection dropped before exit status".into(),
      mutating: true,
    });
    let text = plain(&render_event(&event, &options()));
    assert!(text.contains("[needs check] exec"), "{text}");
    assert!(text.contains("may have changed state"), "{text}");
    assert!(!text.contains("failed"), "{text}");
  }

  #[test]
  fn unknown_read_only_completion_does_not_ask_for_a_check() {
    let event = AgentEvent::ToolUnknown(ToolUnknown {
      call_id: ToolCallId::new(),
      name: "grep".into(),
      why: "stream closed before the final count".into(),
      mutating: false,
    });
    let text = plain(&render_event(&event, &options()));
    assert!(text.contains("[unknown] grep"), "{text}");
    assert!(!text.contains("needs check"), "{text}");
  }

  #[test]
  fn tool_arguments_separate_operation_path_and_value() {
    let event = AgentEvent::ToolRequested(ToolRequested {
      call_id: ToolCallId::new(),
      name: "read".into(),
      arguments: serde_json::json!({ "path": "crates/pi-rs-tui/src/lib.rs", "offset": 10 }),
      read_only: true,
    });
    let line = &render_event(&event, &options())[0];
    let roles: Vec<Role> = line.segments.iter().map(|s| s.role).collect();
    assert!(roles.contains(&Role::Operation), "{roles:?}");
    assert!(roles.contains(&Role::Path), "{roles:?}");
    assert!(roles.contains(&Role::Argument), "{roles:?}");
    assert!(line.plain().contains("read-only"));
  }

  #[test]
  fn mutating_requests_are_flagged() {
    let event = AgentEvent::ToolRequested(ToolRequested {
      call_id: ToolCallId::new(),
      name: "write".into(),
      arguments: serde_json::json!({ "path": "out.txt" }),
      read_only: false,
    });
    assert!(plain(&render_event(&event, &options())).contains("mutating"));
  }

  #[test]
  fn rare_events_are_emphasised_and_routine_ones_are_not() {
    let failover = AgentEvent::ModelFailover(ModelFailover {
      from: model(),
      to: ModelRef::new("backup", "small"),
      kind: ModelFailureKind::ProviderUnavailable,
      gaps: vec![CapabilityGap::Images],
      compacted: true,
    });
    let lines = render_event(&failover, &options());
    assert!(
      lines[0]
        .segments
        .iter()
        .any(|s| s.role == Role::Rare && s.text.contains("backup/small")),
      "{:?}",
      lines[0].segments
    );
    // The label names what happened; rarity is the role's job. A `[rare]` prefix
    // would describe the rendering policy instead of the event.
    assert!(
      plain(&lines).starts_with("[failover] "),
      "{}",
      plain(&lines)
    );

    let retry = AgentEvent::ModelRetry(pi_rs_core::ModelRetry {
      attempt: 2,
      max_attempts: 3,
      kind: ModelFailureKind::RateLimited,
      retry_after_ms: Some(250),
      will_failover: true,
    });
    let retry_line = plain(&render_event(&retry, &options()));
    assert!(retry_line.starts_with("[retry] "), "{retry_line}");
    assert!(retry_line.contains("2 of 3"), "{retry_line}");

    let routine = AgentEvent::ModelRequestStarted(pi_rs_core::ModelRequestStarted {
      epoch: 0,
      model: model(),
      message_count: 3,
      context_tokens_est: 1200,
      tools_exposed: 5,
    });
    let routine_lines = render_event(&routine, &options());
    assert!(
      !routine_lines[0]
        .segments
        .iter()
        .any(|s| s.role == Role::Rare || s.role == Role::Error),
      "{:?}",
      routine_lines[0].segments
    );
  }

  #[test]
  fn session_start_reports_model_and_provenance_capability() {
    let event = AgentEvent::SessionStarted(SessionStarted {
      working_dir: "/repo".into(),
      model: model(),
      capabilities: ModelCapabilities {
        text: true,
        images: false,
        tools: true,
        exposed_reasoning: ReasoningExposure::ProviderSummary,
        context_window: 131_072,
        max_output_tokens: None,
      },
      resumed: false,
    });
    let lines = render_event(&event, &options());
    let text = plain(&lines);
    assert!(text.contains("local/qwen"), "{text}");
    assert!(text.contains("/repo"), "{text}");
    assert!(text.contains("131,072"), "{text}");
    assert!(text.contains("provider summary"), "{text}");
    assert!(
      lines[0]
        .segments
        .iter()
        .any(|s| s.role == Role::ReasoningProviderSummary),
      "{:?}",
      lines[0].segments
    );
  }

  #[test]
  fn every_event_variant_renders_at_least_one_line() {
    let events = all_events();
    assert!(events.len() >= 21, "expected full coverage of 21");
    for event in &events {
      let lines = render_event(event, &options());
      assert!(!lines.is_empty(), "{event:?} rendered nothing");
    }
  }

  #[test]
  fn rendering_is_deterministic_and_palette_only_changes_escapes() {
    let events = all_events();
    for event in &events {
      assert_eq!(
        render_event(event, &options()),
        render_event(event, &options())
      );
      let colored = TranscriptOptions {
        palette: Palette::colored(),
        ..options()
      };
      let colored_lines = render_event(event, &colored);
      let plain_lines = render_event(event, &options());
      assert_eq!(colored_lines.len(), plain_lines.len());
      for (left, right) in colored_lines.iter().zip(&plain_lines) {
        assert_eq!(left.plain(), right.plain());
      }
    }
  }

  #[test]
  fn width_budget_is_respected_for_every_event() {
    let events = all_events();
    for width in [1usize, 7, 20, 40, 80] {
      let options = TranscriptOptions { width, ..options() };
      for event in &events {
        for line in render_event(event, &options) {
          assert!(
            line.width() <= width,
            "width {width}: {:?} is {}",
            line.plain(),
            line.width()
          );
        }
      }
    }
  }

  #[test]
  fn diagnostic_filter_drops_info_but_keeps_error() {
    let info = AgentEvent::Diagnostic(pi_rs_core::Diagnostic {
      level: DiagnosticLevel::Info,
      message: "heartbeat".into(),
    });
    let error = AgentEvent::Diagnostic(pi_rs_core::Diagnostic {
      level: DiagnosticLevel::Error,
      message: "sink down".into(),
    });
    let options = TranscriptOptions {
      diagnostics: DiagnosticFilter::WarnAndError,
      ..options()
    };
    assert!(render_event(&info, &options).is_empty());
    assert_eq!(render_event(&error, &options).len(), 1);
    let text = plain(&render_event(&error, &options));
    assert!(text.contains("sink down"), "{text}");
  }

  #[test]
  fn reduced_tool_output_shows_its_recovery_reference() {
    let event = AgentEvent::ToolCompleted(ToolCompleted {
      call_id: ToolCallId::new(),
      name: "exec".into(),
      state: pi_rs_core::ToolExecutionState::Succeeded,
      duration_ms: 12,
      status: Some(0),
      reduced: true,
      blob: Some(BlobRef::for_bytes(b"output".as_slice(), None)),
      visible_bytes: 512,
    });
    let text = plain(&render_event(&event, &options()));
    assert!(text.contains("reduced"), "{text}");
    assert!(text.contains("blobs/"), "{text}");
    assert!(text.contains("512 B"), "{text}");
  }

  #[test]
  fn context_reduction_states_both_sizes_and_a_recovery_pointer() {
    let event = AgentEvent::ContextReduced(pi_rs_core::ContextReduced {
      reason: ReductionReason::OversizedToolOutput { limit_bytes: 4096 },
      original_bytes: 40_960,
      visible_bytes: 2048,
      blob: Some(BlobRef::for_bytes(b"x".as_slice(), None)),
      recovery_ref: Some("blobs/aa:aa".into()),
      tool_call_id: Some(ToolCallId::new()),
    });
    let text = plain(&render_event(&event, &options()));
    assert!(text.contains("40 KiB → 2.0 KiB"), "{text}");
    assert!(text.contains("recover blobs/aa:aa"), "{text}");
  }

  #[test]
  fn a_reduction_that_cannot_be_undone_says_so() {
    // The absence of a pointer is information, not a formatting gap.
    let event = AgentEvent::ContextReduced(pi_rs_core::ContextReduced {
      reason: ReductionReason::RecentTargetExceeded { target_tokens: 76 },
      original_bytes: 12_000,
      visible_bytes: 300,
      blob: None,
      recovery_ref: None,
      tool_call_id: None,
    });
    let text = plain(&render_event(&event, &options()));
    assert!(text.contains("not recoverable"), "{text}");
    assert!(!text.contains("recover "), "{text}");
  }

  #[test]
  fn epoch_events_are_attributable_to_a_model() {
    let event = AgentEvent::ModelEpochStarted(ModelEpochStarted {
      epoch: 1,
      model: ModelRef::new("backup", "small"),
      reason: EpochReason::AutomaticFailover,
      capabilities: ModelCapabilities::text_only(8192),
    });
    let text = plain(&render_event(&event, &options()));
    assert!(text.contains("backup/small"), "{text}");
    assert!(text.contains("automatic failover"), "{text}");
  }

  #[test]
  fn checkpoints_render_their_path_as_a_path() {
    let event = AgentEvent::CheckpointCreated(pi_rs_core::CheckpointCreated {
      checkpoint_id: CheckpointId::new(),
      capsule_version: 1,
      summarized_events: 42,
      path: "state/sessions/abc/checkpoints/1.json".into(),
    });
    let line = &render_event(&event, &options())[0];
    assert!(
      line
        .segments
        .iter()
        .any(|s| s.role == Role::Path && s.text.contains("checkpoints")),
      "{:?}",
      line.segments
    );
  }

  fn all_events() -> Vec<AgentEvent> {
    vec![
      AgentEvent::SessionStarted(SessionStarted {
        working_dir: "/repo".into(),
        model: model(),
        capabilities: ModelCapabilities::text_only(32_000),
        resumed: true,
      }),
      AgentEvent::UserMessage(pi_rs_core::UserMessage {
        text: "fix the bug".into(),
        attachments: 1,
      }),
      AgentEvent::ModelRequestStarted(pi_rs_core::ModelRequestStarted {
        epoch: 0,
        model: model(),
        message_count: 3,
        context_tokens_est: 1200,
        tools_exposed: 5,
      }),
      reasoning(ReasoningProvenance::Native),
      AgentEvent::AssistantDelta(pi_rs_core::AssistantDelta {
        text: "here".into(),
        chunk_index: 0,
      }),
      AgentEvent::ModelRequestCompleted(pi_rs_core::ModelRequestCompleted {
        epoch: 0,
        model: model(),
        finish_reason: Some("stop".into()),
        input_tokens: Some(10),
        output_tokens: Some(5),
        duration_ms: 1500,
        tool_calls: 2,
        reasoning_provenance: Some(ReasoningProvenance::Reconstructed),
      }),
      AgentEvent::ModelRetry(pi_rs_core::ModelRetry {
        attempt: 1,
        max_attempts: 3,
        kind: ModelFailureKind::RateLimited,
        retry_after_ms: Some(500),
        will_failover: true,
      }),
      AgentEvent::ModelFailover(ModelFailover {
        from: model(),
        to: ModelRef::new("backup", "small"),
        kind: ModelFailureKind::Transport,
        gaps: vec![CapabilityGap::ContextWindow {
          required: 100,
          available: 10,
        }],
        compacted: false,
      }),
      AgentEvent::ModelEpochStarted(ModelEpochStarted {
        epoch: 1,
        model: ModelRef::new("backup", "small"),
        reason: EpochReason::AutomaticFailover,
        capabilities: ModelCapabilities::text_only(8192),
      }),
      AgentEvent::ToolRequested(ToolRequested {
        call_id: ToolCallId::new(),
        name: "read".into(),
        arguments: serde_json::json!({ "path": "src/main.rs", "offset": 1 }),
        read_only: true,
      }),
      AgentEvent::ToolStarted(pi_rs_core::ToolStarted {
        call_id: ToolCallId::new(),
        name: "read".into(),
      }),
      AgentEvent::ToolCompleted(ToolCompleted {
        call_id: ToolCallId::new(),
        name: "exec".into(),
        state: pi_rs_core::ToolExecutionState::Succeeded,
        duration_ms: 1200,
        status: Some(101),
        reduced: true,
        blob: Some(BlobRef::for_bytes(b"o".as_slice(), None)),
        visible_bytes: 900,
      }),
      AgentEvent::ToolFailed(pi_rs_core::ToolFailed {
        call_id: ToolCallId::new(),
        name: "edit".into(),
        message: "old text not found".into(),
        duration_ms: 3,
        status: None,
      }),
      AgentEvent::ToolUnknown(ToolUnknown {
        call_id: ToolCallId::new(),
        name: "exec".into(),
        why: "no exit status".into(),
        mutating: true,
      }),
      AgentEvent::ExternalContextRetrieved(pi_rs_core::ExternalContextRetrieved {
        source: pi_rs_core::ExternalContextSource {
          provider: "rkb".into(),
          resource_id: "rkb:doc/42".into(),
          provenance: "user-library".into(),
        },
        citation: Some("[1]".into()),
        bytes: 2048,
        inline: true,
      }),
      AgentEvent::ContextReduced(pi_rs_core::ContextReduced {
        reason: ReductionReason::RecentTargetExceeded {
          target_tokens: 12000,
        },
        original_bytes: 40_960,
        visible_bytes: 2048,
        blob: Some(BlobRef::for_bytes(b"x".as_slice(), None)),
        recovery_ref: Some("blobs/aa:aa".into()),
        tool_call_id: None,
      }),
      AgentEvent::ContextCompactionStarted(pi_rs_core::ContextCompactionStarted {
        level: pi_rs_core::ContextLevel::L1Ordinary,
        reason: "context budget".into(),
      }),
      AgentEvent::ContextCompactionCompleted(pi_rs_core::ContextCompactionCompleted {
        level: pi_rs_core::ContextLevel::L2Phase,
        removed_messages: 12,
        retained_messages: 30,
        context_epoch: 2,
      }),
      AgentEvent::CheckpointCreated(pi_rs_core::CheckpointCreated {
        checkpoint_id: CheckpointId::new(),
        capsule_version: 1,
        summarized_events: 4,
        path: "state/sessions/x/checkpoints/1.json".into(),
      }),
      AgentEvent::TurnCompleted(pi_rs_core::TurnCompleted {
        status: TurnStatus::Failed {
          kind: ModelFailureKind::Timeout,
        },
        duration_ms: 65_000,
      }),
      AgentEvent::Diagnostic(pi_rs_core::Diagnostic {
        level: DiagnosticLevel::Warn,
        message: "backup model is slower".into(),
      }),
      AgentEvent::SessionEnded(pi_rs_core::SessionEnded {
        reason: SessionEndReason::Fatal {
          message: "sink failed".into(),
        },
      }),
    ]
  }
}
