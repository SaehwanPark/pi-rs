//! The status line: a snapshot in, a [`RenderLine`] out.
//!
//! This is a projection, not a surface. It reads no clock, no environment, no
//! terminal, and no runtime state; the caller supplies everything it knows and
//! gets back role-tagged segments sized to the columns it has. Same snapshot, same
//! line, which is what lets a test assert on it without a terminal.
//!
//! # What this line is not
//!
//! It is not the turn-status surface. While the loop is drawing, a busy session can
//! only be in [`Activity::Running`]; a turn that was cancelled or failed is reported
//! by the loop itself, in the transcript, where there is room to say what happened.
//! Turning [`Activity::Waiting`] into "the last turn failed" would make the line
//! claim a fact it cannot see.

use crate::{
  line::{RenderLine, Segment},
  style::Role,
  width::{display_width, truncate},
};

/// The separator between status segments. Muted so the facts carry the line.
const SEPARATOR: &str = " · ";

/// The narrowest budget that can still name a model.
///
/// One column is either the first letter of the name or the ellipsis, and neither
/// answers "which model is answering", so at that budget the model is dropped and
/// the activity word is what the line says.
const MIN_MODEL_COLUMNS: usize = 2;

/// What the loop is doing, as far as the status line can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
  /// No turn in flight.
  Waiting,
  /// A turn is in flight. Cancellation and failure are the loop's to report.
  Running,
}

impl Activity {
  /// The word a reader sees for this activity.
  fn word(self) -> &'static str {
    match self {
      Activity::Waiting => "idle",
      Activity::Running => "working",
    }
  }
}

/// Everything the status line knows.
///
/// A plain data snapshot rather than a borrow of session or runtime state: the
/// caller decides how fresh it is, and this module cannot be blamed for staleness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status<'a> {
  /// Model answering, or about to.
  pub model: &'a str,
  /// Loop activity.
  pub activity: Activity,
  /// Turns so far. `0` renders no turn segment at all.
  pub turns: usize,
  /// Columns available to the line. `0` means no budget, not "unbounded".
  pub columns: usize,
  /// What the loop offers the user, e.g. a key hint. `None` renders no separator.
  pub hint: Option<&'a str>,
}

/// The status line for `status`, never empty.
///
/// Wide, the shape is `<model> · <activity> · <n> turns · <hint>`; empty and
/// absent parts take their separator with them. Narrow, whole segments are dropped
/// rather than wrapped — the hint first, then the turn count, then the model is
/// truncated. Below the room to name a model, the activity word alone survives:
/// it is the honest floor, and an empty line is not an option.
pub fn line(status: &Status<'_>) -> RenderLine {
  let activity = Segment::new(status.activity.word(), Role::Status);
  let turns = if status.turns > 0 {
    Some(Segment::new(format!("{} turns", status.turns), Role::Meta))
  } else {
    None
  };
  let hint = status.hint.map(|hint| Segment::new(hint, Role::Muted));

  let model = Segment::new(status.model, Role::Meta);
  let mut parts: Vec<&Segment> = vec![&model, &activity];
  if let Some(turns) = &turns {
    parts.push(turns);
  }
  if let Some(hint) = &hint {
    parts.push(hint);
  }

  // The droppable parts are the trailing ones, in load-bearing order: the hint is
  // advice, the turn count is a fact the transcript already carries, the model is
  // the reason the line exists.
  for _ in 0..parts.len() - 2 {
    let line = joined(&parts);
    if line.width() <= status.columns {
      return line;
    }
    parts.pop();
  }
  let line = joined(&parts);
  if line.width() <= status.columns {
    return line;
  }

  // Only model and activity remain, and they do not fit together. Give the model
  // what room the activity word and its separator do not use; where that is not
  // enough to name a model, the activity word stands alone. A one-word line is
  // not truncated: there is no shorter spelling of it that still says anything.
  let budget = status
    .columns
    .saturating_sub(display_width(SEPARATOR) + activity.width());
  if budget >= MIN_MODEL_COLUMNS {
    // `joined` so a model that truncates to nothing takes its separator with it.
    let model = Segment::new(truncate(status.model, budget), Role::Meta);
    return joined(&[&model, &activity]);
  }
  RenderLine::text(activity.text.as_str(), Role::Status)
}

/// `parts` joined by muted [`SEPARATOR`]. Parts with nothing to say, and their
/// separators, are left out rather than rendered as punctuation.
fn joined(parts: &[&Segment]) -> RenderLine {
  let mut line = RenderLine::new();
  for part in parts.iter().filter(|part| !part.text.is_empty()) {
    if !line.is_empty() {
      line.push(SEPARATOR, Role::Muted);
    }
    line.push(&part.text, part.role);
  }
  line
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::width::MIN_COLUMN;

  const MODEL: &str = "vulcan-70b";
  const HINT: &str = "? for commands";

  fn status(columns: usize) -> Status<'static> {
    Status {
      model: MODEL,
      activity: Activity::Waiting,
      turns: 3,
      columns,
      hint: Some(HINT),
    }
  }

  /// Role of the first segment carrying `needle`, so tests assert on segments
  /// rather than on ANSI. A separator merged with the hint still reads as muted.
  fn role_of(line: &RenderLine, needle: &str) -> Option<Role> {
    line
      .segments
      .iter()
      .find(|segment| segment.text.contains(needle))
      .map(|segment| segment.role)
  }

  #[test]
  fn wide_renders_every_segment_in_order() {
    let line = line(&status(80));
    assert_eq!(line.plain(), "vulcan-70b · idle · 3 turns · ? for commands");
  }

  #[test]
  fn segments_carry_their_assigned_roles() {
    let line = line(&status(80));
    assert_eq!(role_of(&line, MODEL), Some(Role::Meta));
    assert_eq!(role_of(&line, "idle"), Some(Role::Status));
    assert_eq!(role_of(&line, "3 turns"), Some(Role::Meta));
    assert_eq!(role_of(&line, HINT), Some(Role::Muted));
    assert_eq!(role_of(&line, SEPARATOR), Some(Role::Muted));

    // Segment order is the reading order. `push` merges same-role neighbours, so
    // the last separator and the muted hint are one segment.
    let roles: Vec<Role> = line.segments.iter().map(|s| s.role).collect();
    assert_eq!(
      roles,
      [
        Role::Meta,
        Role::Muted,
        Role::Status,
        Role::Muted,
        Role::Meta,
        Role::Muted
      ]
    );
  }

  #[test]
  fn running_says_working() {
    let mut status = status(80);
    status.activity = Activity::Running;
    assert!(line(&status).plain().starts_with("vulcan-70b · working"));
  }

  #[test]
  fn zero_turns_renders_no_turn_segment() {
    let mut status = status(80);
    status.turns = 0;
    assert_eq!(line(&status).plain(), "vulcan-70b · idle · ? for commands");
  }

  #[test]
  fn absent_hint_renders_no_dangling_separator() {
    let mut status = status(80);
    status.hint = None;
    assert_eq!(line(&status).plain(), "vulcan-70b · idle · 3 turns");

    // An empty hint is an absent hint, not a piece of punctuation.
    status.hint = Some("");
    assert_eq!(line(&status).plain(), "vulcan-70b · idle · 3 turns");
  }

  #[test]
  fn narrow_drops_the_hint_first() {
    // 27 columns is the line without the hint; the turn count is still affordable.
    let line = line(&status(30));
    assert_eq!(line.plain(), "vulcan-70b · idle · 3 turns");
    assert!(line.width() <= 30);
  }

  #[test]
  fn narrow_drops_the_turn_count_next() {
    let line = line(&status(24));
    assert_eq!(line.plain(), "vulcan-70b · idle");
    assert!(line.width() <= 24);
  }

  #[test]
  fn narrow_truncates_the_model_last() {
    let line = line(&status(MIN_COLUMN));
    assert_eq!(line.plain(), "vulc… · idle");
    assert!(line.width() <= MIN_COLUMN);
  }

  #[test]
  fn model_and_activity_survive_a_min_column_width() {
    let line = line(&status(MIN_COLUMN));
    assert_eq!(role_of(&line, "idle"), Some(Role::Status));
    assert_eq!(role_of(&line, "vulc…"), Some(Role::Meta));
  }

  #[test]
  fn below_the_room_to_name_a_model_the_activity_word_alone_survives() {
    // "working" and a separator leave a single column at `MIN_COLUMN - 1`, and a
    // single column names no model.
    let mut status = status(MIN_COLUMN - 1);
    status.activity = Activity::Running;
    let line = line(&status);
    assert_eq!(line.plain(), "working");
    assert_eq!(role_of(&line, "working"), Some(Role::Status));
  }

  #[test]
  fn an_empty_model_takes_its_separator_with_it() {
    let mut wide = status(20);
    wide.model = "";
    assert_eq!(line(&wide).plain(), "idle · 3 turns");

    let mut narrow = status(MIN_COLUMN);
    narrow.model = "";
    assert_eq!(line(&narrow).plain(), "idle");
  }

  #[test]
  fn the_narrowest_line_is_never_empty() {
    assert_eq!(line(&status(0)).plain(), "idle");
    assert_eq!(line(&status(1)).plain(), "idle");
    let mut status = status(1);
    status.model = "";
    status.turns = 0;
    status.hint = None;
    assert!(!line(&status).is_empty());
  }
}
