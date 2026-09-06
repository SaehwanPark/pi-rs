//! The live surface: a streaming transcript with two destinations.
//!
//! The split is the contract the one-shot command already promised:
//!
//! ```text
//! assistant prose  → stdout   (raw: unwrapped, undecorated, byte-faithful)
//! everything else  → stderr   (labelled, wrapped, optionally coloured)
//! ```
//!
//! Assistant text is *not* decorated even when colour is on. Piping an answer
//! `> out.md` must produce the answer, and a renderer that wraps stdout to the
//! current terminal width produces different bytes on a phone than on a
//! workstation, which makes the same command non-portable for no user benefit.
//!
//! Reasoning is the opposite case: it must always be labelled, so a live block
//! buffers until it can emit a labelled line. That buffering is bounded by the
//! column budget, never by "until the model stops talking".

use std::io::{self, Write};

use pi_rs_core::{AgentEvent, ModelRef, ReasoningProvenance, ToolExecutionState};

use crate::{
  format::SEPARATOR,
  line::{NarrowDecoration, RenderLine},
  style::{Palette, Role},
  transcript::{
    DiagnosticFilter, TranscriptOptions, completion_label, reasoning_label, render_event,
    tool_request_line,
  },
  width::{MIN_COLUMN, display_width},
};

/// The streaming half of the surface.
#[derive(Debug)]
pub struct Surface<O: Write, E: Write> {
  out: O,
  err: E,
  options: TranscriptOptions,
  block: Block,
  /// `true` when stdout has content that has not been newline-terminated. The
  /// surface owns this so that an answer interrupted by a tool call still ends a
  /// line, and an answer that ended cleanly is not followed by a blank line.
  out_open: bool,
}

#[derive(Debug, Default)]
enum Block {
  #[default]
  Idle,
  Assistant,
  Reasoning(ReasoningBlock),
}

#[derive(Debug)]
struct ReasoningBlock {
  provenance: ReasoningProvenance,
  pending: String,
  chunks: u32,
  chars: u64,
  emitted: bool,
}

impl<O: Write, E: Write> Surface<O, E> {
  pub fn new(out: O, err: E, options: TranscriptOptions) -> Self {
    Self {
      out,
      err,
      options,
      block: Block::Idle,
      out_open: false,
    }
  }

  pub fn options(&self) -> TranscriptOptions {
    self.options
  }

  pub fn palette(&self) -> Palette {
    self.options.palette
  }

  pub fn into_writers(self) -> (O, E) {
    (self.out, self.err)
  }

  /// Render one canonical event. Streaming variants take the incremental path;
  /// everything else renders through [`render_event`] so live and recorded output
  /// cannot drift apart.
  pub fn event(&mut self, event: &AgentEvent) -> io::Result<()> {
    match event {
      AgentEvent::AssistantDelta(delta) => self.text_delta(&delta.text),
      AgentEvent::ReasoningDelta(delta) => self.reasoning(&delta.text, delta.provenance),
      _ => {
        self.close_block()?;
        // A streamed-lifecycle event belongs to this surface and is judged by the rule
        // the streaming methods apply; everything else is judged by whether it is news.
        // Without this split the generic entry point would ignore verbosity entirely
        // while the named methods honoured it.
        let allowed = if is_streamed(event) {
          self.shows_news(routine_stream(event))
        } else {
          self.options.diagnostics.shows(event)
        };
        if !allowed {
          return Ok(());
        }
        self.lines(render_event(event, &self.options))
      }
    }
  }

  /// The user's own text.
  pub fn user_message(&mut self, text: &str) -> io::Result<()> {
    self.close_block()?;
    if !self.shows_news(true) {
      return Ok(());
    }
    let mut line = RenderLine::new();
    line.push("> ", Role::Prompt);
    line.push(text, Role::UserText);
    self.lines(wrap(vec![line], &self.options))
  }

  /// One line per model request.
  ///
  /// Gated on the verbosity level rather than printed unconditionally: three requests
  /// in one turn is three identical lines saying that machinery ran, which is the
  /// noise a calm transcript exists to remove. The prompt echo, by contrast, is
  /// always printed — one line that marks which turn this is costs little and
  /// recovers the context of everything after it.
  pub fn request_started(&mut self, model: &ModelRef) -> io::Result<()> {
    if !self.options.diagnostics.shows_routine() {
      return Ok(());
    }
    self.close_block()?;
    let mut line = RenderLine::new();
    line.push("[request] ", Role::Muted);
    line.push(&model.as_key(), Role::Operation);
    self.lines(wrap(vec![line], &self.options))
  }

  /// Assistant prose, streamed. Written through to stdout exactly as received.
  pub fn text_delta(&mut self, text: &str) -> io::Result<()> {
    if text.is_empty() {
      return Ok(());
    }
    self.close_reasoning()?;
    self.block = Block::Assistant;
    self.out.write_all(text.as_bytes())?;
    self.out.flush()?;
    self.out_open = !text.ends_with('\n');
    Ok(())
  }

  /// Reasoning-like text, streamed, always provenance-labelled.
  pub fn reasoning(&mut self, text: &str, provenance: ReasoningProvenance) -> io::Result<()> {
    if !self.options.show_reasoning || text.is_empty() {
      return Ok(());
    }
    if !self.shows_news(true) {
      // Suppressed before it is buffered: a surface that accumulates text it will never
      // write trades memory for nothing.
      return Ok(());
    }
    // A provenance change is a new block: one block may not mix "the model said"
    // with "a provider summarised", and merging them is the exact collapse this
    // type exists to prevent.
    let restart = match &self.block {
      Block::Reasoning(existing) => existing.provenance != provenance,
      _ => true,
    };
    if restart {
      self.close_reasoning()?;
      self.block = Block::Reasoning(ReasoningBlock {
        provenance,
        pending: String::new(),
        chunks: 0,
        chars: 0,
        emitted: false,
      });
    }
    let Block::Reasoning(block) = &mut self.block else {
      return Ok(());
    };
    block.chunks += 1;
    block.chars += text.chars().count() as u64;
    if self.options.collapse_reasoning {
      // Nothing is written yet, so nothing needs un-writing: the honest collapse
      // is to never emit the body and to report at close what was withheld.
      return Ok(());
    }
    block.pending.push_str(text);
    self.drain_reasoning()
  }

  /// A tool call the model just asked for.
  ///
  /// Rendered through the same builder as the recorded [`pi_rs_core::ToolRequested`]
  /// event, so watching a turn and reading its session file later describe the same
  /// request with the same segments.
  pub fn tool_requested(
    &mut self,
    name: &str,
    arguments: &serde_json::Value,
    read_only: bool,
  ) -> io::Result<()> {
    self.close_block()?;
    if !self.shows_news(true) {
      return Ok(());
    }
    self.lines(wrap(
      vec![tool_request_line(name, arguments, read_only)],
      &self.options,
    ))
  }

  /// Intermediate tool output. Not a canonical event — tool chunks are transient —
  /// so it is rendered, truncated to the column budget when asked to collapse.
  pub fn tool_progress(&mut self, name: &str, text: &str) -> io::Result<()> {
    self.close_block()?;
    if !self.shows_news(true) {
      return Ok(());
    }
    let mut line = RenderLine::new();
    line.push("[output] ", Role::Muted);
    line.push(name, Role::Operation);
    line.push(" · ", Role::Muted);
    let body = if self.options.collapse_tool_output && self.options.width > 0 {
      crate::width::truncate(text, self.options.width.saturating_sub(line.width()))
    } else {
      text.to_string()
    };
    line.push(&body, Role::ToolOutput);
    self.lines(wrap(vec![line], &self.options))
  }

  /// Terminal lifecycle state for a tool call, spelled out rather than coloured
  /// alone: `unknown` must be readable in a log with colour disabled.
  ///
  /// `mutating` comes from the tool's declared metadata, not from a guess here: the
  /// difference between `[unknown]` and `[needs check]` is whether the user has work
  /// to do.
  pub fn tool_finished(
    &mut self,
    name: &str,
    state: ToolExecutionState,
    mutating: bool,
    refusal: Option<&str>,
  ) -> io::Result<()> {
    self.close_block()?;
    // A success is routine, and a quiet transcript is exactly the log that does not want
    // it. A failure, an unrecorded completion, or a policy refusal is news even there.
    let routine = state == ToolExecutionState::Succeeded && refusal.is_none();
    if !self.shows_news(routine) {
      return Ok(());
    }
    // A refusal that arrived with a succeeded state must not be labelled `[tool ok]`:
    // the runtime recorded that the tool ran as asked, while the policy layer said the
    // request itself was not allowed. Both are true, and the line has to stay readable
    // as bad news either way.
    let refused = refusal.is_some() && state == ToolExecutionState::Succeeded;
    let mut line = crate::transcript::label(if refused {
      "tool refused"
    } else {
      completion_label(state, mutating)
    });
    line.push(name, Role::Operation);
    line.push(SEPARATOR, Role::Muted);
    let role = match state {
      ToolExecutionState::Succeeded => Role::StateOk,
      ToolExecutionState::Failed => Role::StateFailed,
      ToolExecutionState::Unknown => Role::StateUnknown,
      _ => Role::Meta,
    };
    if state == ToolExecutionState::Unknown {
      // The label already said `unknown`, or said `needs check` when state may have
      // changed; repeating the state word would bury the warning that follows.
      line.push("completion not recorded", Role::StateUnknown);
    } else if !refused {
      line.push(state.as_str(), role);
    }
    if let Some(reason) = refusal {
      line.push(" · ", Role::Muted);
      line.push(reason, Role::Warning);
    }
    self.lines(wrap(vec![line], &self.options))
  }

  /// Whether a transcript line is displayed at the current level.
  ///
  /// One axis decides both the live surface and the recorded transcript: how much of a
  /// turn counts as news. Assistant prose is not on that axis — it is the answer, it is
  /// written to stdout, and no display setting suppresses it.
  fn shows_news(&self, routine: bool) -> bool {
    if !routine {
      return !matches!(self.options.diagnostics, DiagnosticFilter::None);
    }
    matches!(
      self.options.diagnostics,
      DiagnosticFilter::All | DiagnosticFilter::State
    )
  }

  /// End the current block and flush both writers. Idempotent.
  pub fn finish(&mut self) -> io::Result<()> {
    self.close_block()?;
    self.out.flush()?;
    self.err.flush()
  }

  fn close_block(&mut self) -> io::Result<()> {
    self.close_reasoning()?;
    if self.out_open {
      self.out.write_all(b"\n")?;
      self.out_open = false;
    }
    self.block = Block::Idle;
    Ok(())
  }

  fn close_reasoning(&mut self) -> io::Result<()> {
    let Block::Reasoning(block) = std::mem::take(&mut self.block) else {
      self.block = Block::Idle;
      return Ok(());
    };
    let provenance = block.provenance;
    if self.options.collapse_reasoning {
      if block.chunks > 0 {
        let mut line = RenderLine::new();
        line.push(&reasoning_label(provenance), Role::Muted);
        line.push(
          &format!("collapsed ({} chars, {} chunks)", block.chars, block.chunks),
          crate::transcript::reasoning_role(provenance),
        );
        self.lines(wrap(vec![line], &self.options))?;
      }
      self.block = Block::Idle;
      return Ok(());
    }
    let ReasoningBlock {
      mut pending,
      mut emitted,
      ..
    } = block;
    let width = self.options.width;
    while let Some(piece) =
      take_complete(&mut pending, reasoning_budget(provenance, emitted, width))
    {
      self.emit_reasoning(&piece, provenance, &mut emitted, width)?;
    }
    // The tail of a streamed block has no trailing newline and is shorter than a
    // column, so `take_complete` will never release it. Dropping it would silently
    // lose part of a thought; holding it would stall the transcript.
    if !pending.is_empty() {
      self.emit_reasoning(&pending, provenance, &mut emitted, width)?;
      pending.clear();
    }
    // Every emitted piece is written as a complete line, including the tail flush
    // above, so there is no dangling line to terminate here. Adding a blank line
    // would make the live transcript disagree with the recorded one by one line.
    let _ = emitted;
    self.block = Block::Idle;
    Ok(())
  }

  /// Emit every complete line currently buffered in the open reasoning block.
  fn drain_reasoning(&mut self) -> io::Result<()> {
    let width = self.options.width;
    let (provenance, mut emitted, mut pending) = {
      let Block::Reasoning(block) = &mut self.block else {
        return Ok(());
      };
      (
        block.provenance,
        block.emitted,
        std::mem::take(&mut block.pending),
      )
    };
    while let Some(piece) =
      take_complete(&mut pending, reasoning_budget(provenance, emitted, width))
    {
      self.emit_reasoning(&piece, provenance, &mut emitted, width)?;
    }
    if let Block::Reasoning(block) = &mut self.block {
      block.pending = pending;
      block.emitted = emitted;
    }
    Ok(())
  }

  fn emit_reasoning(
    &mut self,
    text: &str,
    provenance: ReasoningProvenance,
    emitted: &mut bool,
    width: usize,
  ) -> io::Result<()> {
    let mut line = RenderLine::new();
    if *emitted {
      let indent = continuation_indent(&reasoning_label(provenance), width);
      if indent > 0 {
        line.push(&" ".repeat(indent), Role::Muted);
      }
    } else {
      line.push(&reasoning_label(provenance), Role::Muted);
    }
    line.push(text, crate::transcript::reasoning_role(provenance));
    *emitted = true;
    self
      .err
      .write_all(line.render(self.options.palette).as_bytes())?;
    self.err.write_all(b"\n")
  }

  fn lines(&mut self, lines: Vec<RenderLine>) -> io::Result<()> {
    for line in lines {
      self
        .err
        .write_all(line.render(self.options.palette).as_bytes())?;
      self.err.write_all(b"\n")?;
    }
    self.err.flush()
  }
}

/// Columns available for reasoning text beside the label column.
///
/// Wrapping must budget against the line, not against the text: a 40-column
/// transcript with a 12-column label column has 27 columns of body, and a budget
/// that ignored the label would put 40 columns of reasoning into a 40-column
/// terminal and overflow it.
fn reasoning_budget(provenance: ReasoningProvenance, emitted: bool, width: usize) -> usize {
  if width == 0 {
    return 0;
  }
  // `reasoning_label` includes the `[...] ` separator, so the reserve is the whole
  // label width rather than label-plus-space.
  let label = reasoning_label(provenance);
  let reserve = if emitted {
    continuation_indent(&label, width)
  } else {
    display_width(&label)
  };
  width.saturating_sub(reserve)
}

/// Indent continuation lines so a multi-line block reads as one indented body.
/// wrapped thought reads as one block. Dropped below the narrow-column floor
/// where indentation would cost more than it explains.
fn continuation_indent(label: &str, width: usize) -> usize {
  if width == 0 || width < MIN_COLUMN {
    return 0;
  }
  display_width(label).min(width.saturating_sub(4))
}

fn wrap(lines: Vec<RenderLine>, options: &TranscriptOptions) -> Vec<RenderLine> {
  if options.width == 0 {
    return lines;
  }
  let decoration = if options.width >= MIN_COLUMN {
    NarrowDecoration::Keep
  } else {
    NarrowDecoration::Strip
  };
  lines
    .iter()
    .flat_map(|line| line.wrapped(options.width, decoration))
    .collect()
}

/// Whether the live surface is what shows this event's content.
///
/// A caller that streams deltas *and* replays durable events needs one place to say
/// which content it already showed, or the same sentence appears twice: once while
/// it was arriving, once when it was recorded. A caller that never streamed (a
/// replay command) must not use this.
pub fn is_streamed(event: &AgentEvent) -> bool {
  use AgentEvent as E;
  matches!(
    event,
    E::UserMessage(_)
      | E::ModelRequestStarted(_)
      | E::ReasoningDelta(_)
      | E::AssistantDelta(_)
      | E::ToolRequested(_)
      | E::ToolStarted(_)
      | E::ToolCompleted(_)
      | E::ToolFailed(_)
      | E::ToolUnknown(_)
  )
}

/// Whether a streamed-lifecycle event reports routine work rather than news.
///
/// Only the completion state decides this: a request and a success are routine, a
/// failure or an unrecorded completion is not. It is the live mirror of
/// [`DiagnosticFilter::shows`], which makes the same judgement for recorded events.
pub fn routine_stream(event: &AgentEvent) -> bool {
  use AgentEvent as E;
  match event {
    E::ToolFailed(_) | E::ToolUnknown(_) => false,
    E::ToolCompleted(done) => done.state == ToolExecutionState::Succeeded,
    _ => true,
  }
}

/// Take one emittable line out of a streamed buffer.
///
/// A line is complete when it hits a newline. Without one, the buffer is
/// still emit-able once it reaches the column budget: a live block must not
/// hold text hostage waiting for a word boundary that may never arrive. The
/// trailing partial word stays buffered when it *could* still fit on the next
/// line, which keeps ordinary prose word-aligned; a word longer than the line
/// is hard-split rather than buffered forever.
fn take_complete(pending: &mut String, width: usize) -> Option<String> {
  if let Some(index) = pending.find('\n') {
    let line = pending[..index].trim_end_matches('\r').to_string();
    pending.drain(..=index);
    return Some(line);
  }
  if width == 0 {
    return None;
  }
  if display_width(pending) < width {
    return None;
  }
  // Prefer a break at the last whitespace that still fits.
  let mut width_so_far = 0usize;
  let mut last_space: Option<(usize, usize)> = None;
  let mut bytes = 0usize;
  for ch in pending.chars() {
    let ch_width = crate::width::char_width(ch);
    if width_so_far + ch_width > width {
      break;
    }
    width_so_far += ch_width;
    bytes += ch.len_utf8();
    if ch == ' ' {
      last_space = Some((bytes, width_so_far));
    }
  }
  let cut = if let Some((byte, _)) = last_space {
    // Only break at a space when the word before it is what we are flushing: a
    // leading-space-only candidate would emit an empty line.
    if byte > 1 { byte - 1 } else { bytes }
  } else {
    bytes
  };
  let consumed = &pending[..cut];
  let line = consumed.trim_end().to_string();
  let consumed_bytes = consumed.trim_end().len();
  pending.drain(..consumed_bytes);
  // A word break releases the line without the space it broke at, so that space is
  // now at the head of the buffer. Left there, every continuation line of a block
  // sits one column right of the body column.
  if pending.starts_with(' ') {
    pending.remove(0);
  }
  if line.is_empty() {
    // Guard against a stalled buffer: if trimming produced nothing, drop the
    // consumed bytes and try the next chunk rather than looping.
    return take_complete(pending, width);
  }
  Some(line)
}

#[cfg(test)]
mod tests {
  use std::io::Write;

  use pi_rs_core::{
    AssistantDelta, ReasoningProvenance, SessionEndReason, SessionEnded, ToolExecutionState,
    ToolUnknown,
  };

  use super::*;
  use crate::style::Palette;

  /// A surface that writes nowhere, for assertions on ordering rather than bytes.
  fn surface(options: TranscriptOptions) -> Surface<Vec<u8>, Vec<u8>> {
    Surface::new(Vec::new(), Vec::new(), options)
  }

  fn take(surface: Surface<Vec<u8>, Vec<u8>>) -> (String, String) {
    let (out, err) = surface.into_writers();
    (
      String::from_utf8(out).expect("stdout utf-8"),
      String::from_utf8(err).expect("stderr utf-8"),
    )
  }

  #[test]
  fn assistant_text_is_stdout_and_nothing_else_is() {
    let mut surface = surface(TranscriptOptions::default());
    surface.text_delta("the answer\n").unwrap();
    surface
      .event(&AgentEvent::SessionEnded(SessionEnded {
        reason: SessionEndReason::UserExit,
      }))
      .unwrap();
    let (out, err) = take(surface);
    assert_eq!(out, "the answer\n");
    assert_eq!(err, "[session end] user exit\n");
  }

  #[test]
  fn an_interrupted_answer_is_terminated_but_a_finished_one_is_not_padded() {
    let mut interrupted = surface(TranscriptOptions::default());
    interrupted.text_delta("partial").unwrap();
    interrupted
      .tool_finished("read", ToolExecutionState::Succeeded, false, None)
      .unwrap();
    let (out, err) = take(interrupted);
    assert_eq!(out, "partial\n");
    assert_eq!(err, "[tool ok] read · succeeded\n");

    let mut finished = surface(TranscriptOptions::default());
    finished.text_delta("done\n").unwrap();
    finished.finish().unwrap();
    let (out, _) = take(finished);
    assert_eq!(out, "done\n");
  }

  #[test]
  fn stdout_never_gains_escapes_even_with_colour() {
    let options = TranscriptOptions {
      palette: Palette::colored(),
      ..TranscriptOptions::default()
    };
    let mut surface = surface(options);
    surface.text_delta("raw \x1b prose").unwrap();
    surface
      .reasoning("thought", ReasoningProvenance::Native)
      .unwrap();
    surface.finish().unwrap();
    let (out, err) = take(surface);
    assert_eq!(out, "raw \x1b prose\n");
    assert!(err.contains("\x1b["), "{err}");
    assert!(err.starts_with("\x1b[2m[reasoning]"), "{err}");
  }

  #[test]
  fn streamed_reasoning_is_labelled_once_and_indented_after() {
    let mut surface = surface(TranscriptOptions::default());
    for chunk in ["first thought. ", "second thought"] {
      surface
        .reasoning(chunk, ReasoningProvenance::Native)
        .unwrap();
    }
    surface.finish().unwrap();
    let (_, err) = take(surface);
    assert_eq!(err, "[reasoning] first thought. second thought\n");
  }

  #[test]
  fn live_reasoning_wraps_at_the_column_budget_without_losing_text() {
    let options = TranscriptOptions {
      width: 40,
      ..TranscriptOptions::default()
    };
    let mut surface = surface(options);
    for chunk in [
      "considering ",
      "several plausible ",
      "explanations for this ",
      "failure",
    ] {
      surface
        .reasoning(chunk, ReasoningProvenance::Native)
        .unwrap();
    }
    surface.finish().unwrap();
    let (_, err) = take(surface);
    let lines: Vec<&str> = err.trim_end_matches('\n').split('\n').collect();
    assert!(lines.len() > 1, "{lines:?}");
    assert!(lines[0].starts_with("[reasoning] considering"), "{lines:?}");
    // Continuation lines hang under the body column, not under the label.
    assert!(lines[1].starts_with("            p"), "{lines:?}");
    for line in &lines {
      assert!(crate::width::display_width(line) <= 40, "{line:?}");
    }
    let body: String = lines
      .iter()
      .flat_map(|l| l.chars().filter(|c| !c.is_whitespace()))
      .collect();
    assert_eq!(
      body,
      "[reasoning]consideringseveralplausibleexplanationsforthisfailure"
    );
  }

  #[test]
  fn one_reasoning_block_never_mixes_provenance() {
    let mut surface = surface(TranscriptOptions::default());
    surface
      .reasoning("native", ReasoningProvenance::Native)
      .unwrap();
    surface
      .reasoning("summarised", ReasoningProvenance::ProviderSummary)
      .unwrap();
    surface.finish().unwrap();
    let (_, err) = take(surface);
    assert_eq!(err, "[reasoning] native\n[provider summary] summarised\n");
  }

  #[test]
  fn a_collapsed_live_block_never_emits_the_body_it_promised_to_hide() {
    let options = TranscriptOptions {
      collapse_reasoning: true,
      ..TranscriptOptions::default()
    };
    let mut surface = surface(options);
    surface
      .reasoning("secret thought", ReasoningProvenance::Native)
      .unwrap();
    surface
      .reasoning(" more", ReasoningProvenance::Native)
      .unwrap();
    surface.finish().unwrap();
    let (_, err) = take(surface);
    assert_eq!(err, "[reasoning] collapsed (19 chars, 2 chunks)\n");
  }

  #[test]
  fn suppressed_reasoning_is_silent_rather_than_collapsed() {
    let options = TranscriptOptions {
      show_reasoning: false,
      ..TranscriptOptions::default()
    };
    let mut surface = surface(options);
    surface
      .reasoning("hidden", ReasoningProvenance::Native)
      .unwrap();
    surface.finish().unwrap();
    let (out, err) = take(surface);
    assert_eq!(out, "");
    assert_eq!(err, "");
  }

  #[test]
  fn tool_progress_respects_the_column_budget_when_collapsing() {
    let options = TranscriptOptions {
      width: 30,
      collapse_tool_output: true,
      ..TranscriptOptions::default()
    };
    let mut surface = surface(options);
    surface.tool_progress("exec", &"x".repeat(200)).unwrap();
    surface.finish().unwrap();
    let (_, err) = take(surface);
    let line = err.trim_end();
    assert!(crate::width::display_width(line) <= 30, "{line:?}");
    assert!(line.contains("[output] exec"), "{line:?}");
  }

  #[test]
  fn unknown_completion_is_spelled_out_on_stderr() {
    let mut surface = surface(TranscriptOptions::default());
    surface
      .event(&AgentEvent::ToolUnknown(ToolUnknown {
        call_id: pi_rs_core::ToolCallId::new(),
        name: "exec".into(),
        why: "no exit status".into(),
        mutating: true,
      }))
      .unwrap();
    let (out, err) = take(surface);
    assert_eq!(out, "");
    assert!(err.contains("needs check"), "{err}");
    assert!(!err.contains("failed"), "{err}");
  }

  #[test]
  fn a_refusal_is_readable_without_a_colour_palette() {
    let mut surface = surface(TranscriptOptions::default());
    surface
      .tool_finished(
        "write",
        ToolExecutionState::Requested,
        true,
        Some("outside the approved directory"),
      )
      .unwrap();
    let (_, err) = take(surface);
    assert_eq!(
      err,
      "[tool] write · requested · outside the approved directory\n"
    );
  }

  #[test]
  fn deltas_then_a_recorded_completion_do_not_reorder_the_transcript() {
    let mut surface = surface(TranscriptOptions::default());
    surface
      .event(&AgentEvent::AssistantDelta(AssistantDelta {
        text: "a".into(),
        chunk_index: 0,
      }))
      .unwrap();
    surface
      .event(&AgentEvent::AssistantDelta(AssistantDelta {
        text: "b".into(),
        chunk_index: 1,
      }))
      .unwrap();
    surface.user_message("next question").unwrap();
    let (out, err) = take(surface);
    assert_eq!(out, "ab\n");
    assert_eq!(err, "> next question\n");
  }

  #[test]
  fn finish_is_idempotent_and_flushes() {
    let mut surface = surface(TranscriptOptions::default());
    surface.text_delta("x").unwrap();
    surface.finish().unwrap();
    surface.finish().unwrap();
    let (out, err) = take(surface);
    assert_eq!(out, "x\n");
    assert_eq!(err, "");
  }

  #[test]
  fn a_write_failure_is_reported_rather_than_swallowed() {
    /// Fails after the first byte, like a closed pipe.
    struct Broken;
    impl Write for Broken {
      fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = buf;
        Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
      }
      fn flush(&mut self) -> io::Result<()> {
        Ok(())
      }
    }
    let mut surface = Surface::new(Vec::new(), Broken, TranscriptOptions::default());
    surface.text_delta("answer").unwrap();
    let error = surface
      .user_message("still typing")
      .expect_err("a broken stderr must surface");
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
  }

  #[test]
  fn wide_input_is_not_wrapped_when_width_is_zero() {
    let mut surface = surface(TranscriptOptions::default());
    let long = "word ".repeat(200);
    surface.text_delta(&long).unwrap();
    surface.finish().unwrap();
    let (out, err) = take(surface);
    assert_eq!(out, format!("{long}\n"));
    assert_eq!(err, "");
  }

  #[test]
  fn request_started_is_quiet_on_its_own_line() {
    let model = ModelRef::new("local", "qwen");
    let mut surface = surface(TranscriptOptions::default());
    surface.request_started(&model).unwrap();
    let (_, err) = take(surface);
    assert_eq!(err, "[request] local/qwen\n");
  }
  fn level(diagnostics: DiagnosticFilter) -> TranscriptOptions {
    TranscriptOptions {
      diagnostics,
      ..TranscriptOptions::default()
    }
  }

  #[test]
  fn quiet_prints_trouble_and_silences_routine_work() {
    let mut surface = surface(level(DiagnosticFilter::WarnAndError));
    surface.user_message("go").unwrap();
    surface
      .reasoning("thinking", ReasoningProvenance::Native)
      .unwrap();
    surface.text_delta("answer\n").unwrap();
    surface
      .tool_requested("write", &serde_json::json!({ "path": "a.txt" }), false)
      .unwrap();
    surface
      .tool_finished("write", ToolExecutionState::Succeeded, true, None)
      .unwrap();
    let (out, err) = take(surface);
    // The answer is not transcript: no display level suppresses it.
    assert_eq!(out, "answer\n");
    assert_eq!(err, "", "routine work is not news: {err}");
  }

  #[test]
  fn quiet_still_reports_failure_and_refusal() {
    let mut surface = surface(level(DiagnosticFilter::WarnAndError));
    surface
      .tool_finished("write", ToolExecutionState::Failed, true, None)
      .unwrap();
    surface
      .tool_finished("exec", ToolExecutionState::Unknown, true, None)
      .unwrap();
    let (_, err) = take(surface);
    assert!(err.contains("[tool failed] write"), "{err}");
    assert!(err.contains("[needs check] exec"), "{err}");
  }

  #[test]
  fn silent_prints_no_transcript_and_still_the_answer() {
    let mut surface = surface(level(DiagnosticFilter::None));
    surface.user_message("go").unwrap();
    surface
      .reasoning("thinking", ReasoningProvenance::Native)
      .unwrap();
    surface.text_delta("answer\n").unwrap();
    surface
      .tool_requested("write", &serde_json::json!({ "path": "a.txt" }), false)
      .unwrap();
    surface
      .tool_finished("write", ToolExecutionState::Failed, true, None)
      .unwrap();
    let (out, err) = take(surface);
    assert_eq!(err, "");
    assert_eq!(out, "answer\n");
  }

  #[test]
  fn the_generic_event_path_honours_the_level_too() {
    // `event` is what a replaying caller uses. If it ignored the level, one entry point
    // would be calm and the other chatty for the same options.
    let mut surface = surface(level(DiagnosticFilter::WarnAndError));
    surface
      .event(&AgentEvent::SessionEnded(SessionEnded {
        reason: SessionEndReason::UserExit,
      }))
      .unwrap();
    surface
      .event(&AgentEvent::ToolUnknown(ToolUnknown {
        call_id: pi_rs_core::ToolCallId::new(),
        name: "exec".into(),
        why: "process exited without a recorded status".into(),
        mutating: true,
      }))
      .unwrap();
    let (_, err) = take(surface);
    assert!(!err.contains("[session end]"), "{err}");
    assert!(err.contains("[needs check] exec"), "{err}");
  }

  #[test]
  fn a_refusal_never_borrows_the_ok_label() {
    let mut surface = surface(TranscriptOptions::default());
    surface
      .tool_finished(
        "write",
        ToolExecutionState::Succeeded,
        true,
        Some("approval required"),
      )
      .unwrap();
    let (_, err) = take(surface);
    assert!(err.contains("[tool refused] write"), "{err}");
    assert!(!err.contains("succeeded"), "{err}");
    assert!(err.contains("approval required"), "{err}");
  }
}
