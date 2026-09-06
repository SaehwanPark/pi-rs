//! Segmented lines: the renderer's unit of output.
//!
//! A line is a sequence of styled segments rather than a string plus a colour,
//! because the distinctions the surface must express are *within* a line:
//! `read` (operation) `crates/x/lib.rs` (path) `--offset=10` (flag). A
//! line-level colour cannot express that, and string post-processing to recover it
//! is how renderers end up colour-coding their own escape sequences.
//!
//! Plain text is the primary representation. ANSI is a projection of the same
//! structure, produced on demand by [`RenderLine::render`].

use std::fmt;

use crate::{
  style::{Palette, Role},
  width::{MIN_COLUMN, char_width, display_width},
};

/// One run of text with one semantic role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
  pub text: String,
  pub role: Role,
}

impl Segment {
  pub fn new(text: impl Into<String>, role: Role) -> Self {
    Self {
      text: text.into(),
      role,
    }
  }

  /// Display columns of this segment.
  pub fn width(&self) -> usize {
    display_width(&self.text)
  }
}

/// A rendered line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RenderLine {
  pub segments: Vec<Segment>,
}

impl RenderLine {
  pub fn new() -> Self {
    Self::default()
  }

  /// One segment, the common case.
  pub fn text(text: impl Into<String>, role: Role) -> Self {
    Self {
      segments: vec![Segment::new(text, role)],
    }
  }

  /// Append text in `role`, merging with the previous segment when the role is the
  /// same so streamed text does not become one segment per chunk.
  pub fn push(&mut self, text: &str, role: Role) -> &mut Self {
    if text.is_empty() {
      return self;
    }
    match self.segments.last_mut() {
      Some(last) if last.role == role => last.text.push_str(text),
      _ => self.segments.push(Segment::new(text, role)),
    }
    self
  }

  /// Append an already-built line.
  pub fn push_line(&mut self, line: &RenderLine) -> &mut Self {
    for segment in &line.segments {
      self.push(&segment.text, segment.role);
    }
    self
  }

  /// Prepend a string in [`Role::Muted`], used for the `[label]` prefixes that
  /// carry the semantic distinction while the body stays readable in a pipe.
  pub fn with_prefix(&mut self, prefix: &str) -> &mut Self {
    if !prefix.is_empty() {
      self.segments.insert(0, Segment::new(prefix, Role::Muted));
    }
    self
  }

  /// `true` when the line carries no visible columns.
  pub fn is_empty(&self) -> bool {
    self.width() == 0
  }

  /// Display columns across all segments.
  pub fn width(&self) -> usize {
    self.segments.iter().map(|s| s.width()).sum()
  }

  /// Monochrome text: the reference rendering.
  pub fn plain(&self) -> String {
    self.segments.iter().map(|s| s.text.as_str()).collect()
  }

  /// ANSI projection under `palette`.
  pub fn render(&self, palette: Palette) -> String {
    if !palette.is_color() {
      return self.plain();
    }
    let mut out = String::new();
    for segment in &self.segments {
      out.push_str(&palette.paint(segment.role, &segment.text));
    }
    out
  }

  /// Re-wrap to `width` columns, keeping each segment's role on the pieces it
  /// contributes.
  ///
  /// Wrapping a *segmented* line rather than its flat text is what preserves the
  /// operation/argument/path distinction on continuation lines. A wide atom is hard
  /// split at the column budget; the pieces keep their role, which is why a path that
  /// wraps across two lines is still recognisably a path on both.
  ///
  /// Wrapping fills lines rather than flushing them. Wrapping each segment inside its
  /// own budget was the first approach, and it failed twice in the same way: the
  /// separator between two segments landed at a line break and the next segment was
  /// glued to it (`[tool failed]exec\u{b7}` at twenty columns), and a short label
  /// followed by one long body segment got a line to itself with eleven columns of the
  /// answer still unwritten. Filling treats the line as one stream of words with roles
  /// attached, which is both what a reader expects and what keeps every column usable.
  pub fn wrapped(&self, width: usize, narrow_decoration: NarrowDecoration) -> Vec<RenderLine> {
    // `0` is not "one column wide", it is "no budget at all": a piped transcript is
    // written unwrapped, which is the whole point of the width-0 convention.
    if width == 0 || self.width() <= width {
      return vec![self.clone()];
    }
    let width = width.max(1);
    let indent = match narrow_decoration {
      NarrowDecoration::Keep if width >= MIN_COLUMN => leading_columns(self),
      _ => 0,
    };
    let indent = indent.min(width - 1);

    let mut lines: Vec<RenderLine> = Vec::new();
    let mut current = RenderLine::new();
    let mut filled = false;
    // Whitespace is held rather than written: it belongs to a line only if the word it
    // precedes fits there too. This is what keeps a wrapped line from ending in a space,
    // and from starting with one.
    let mut pending: Option<Atom<'_>> = None;

    for atom in atoms(self) {
      if atom.space {
        pending = Some(atom);
        continue;
      }
      let mut rest = atom.text;
      loop {
        let separator = pending.map(|atom| display_width(atom.text)).unwrap_or(0);
        let atom_width = display_width(rest);
        // A held separator counts against the budget: "the word plus the space before
        // it" is the unit that has to fit. Saturating, because a budget this small means
        // the indent or the space has already taken the line.
        let remaining = width.saturating_sub(current.width());
        // A separator that would consume the whole line is dropped rather than written:
        // at four columns of budget, an indent of four is worth less than the word it
        // would leave out. Whitespace is the cheapest thing on a line to give up.
        let separator = if separator >= remaining {
          pending = None;
          0
        } else {
          separator
        };
        if filled && atom_width + separator > remaining {
          lines.push(std::mem::take(&mut current));
          start_continuation(&mut current, indent);
          filled = false;
          pending = None;
          continue;
        }
        if !filled && atom_width + separator > width {
          // Nothing on this line, and the word alone is wider than the budget: the only
          // way to make progress is to split inside it.
          let (head, tail) = split_prefix(rest, remaining.saturating_sub(separator).max(1));
          emit(&mut current, &mut pending);
          current.push(&head, atom.role);
          lines.push(std::mem::take(&mut current));
          start_continuation(&mut current, indent);
          filled = false;
          pending = None;
          rest = tail;
          if rest.is_empty() {
            break;
          }
          continue;
        }
        emit(&mut current, &mut pending);
        current.push(rest, atom.role);
        filled = true;
        break;
      }
    }
    if !current.is_empty() {
      lines.push(current);
    }
    if lines.is_empty() {
      lines.push(RenderLine::new());
    }
    lines
  }
}

/// Write held whitespace, now that a word follows it on the same line.
fn emit<'a>(current: &mut RenderLine, pending: &mut Option<Atom<'a>>) {
  if let Some(atom) = pending.take() {
    current.push(atom.text, atom.role);
  }
}

/// A line built for content after the first: the indent is decoration, not content.
fn start_continuation(line: &mut RenderLine, indent: usize) {
  if indent > 0 {
    line.push(&" ".repeat(indent), Role::Muted);
  }
}

/// Split off the longest character prefix that fits `budget` columns.
///
/// This path only exists for an atom with no space in it that is wider than an entire
/// line, which is a path or a token someone pasted. Splitting between characters keeps
/// the text intact and the widths honest; a combining mark may end up on the next line
/// from its base character, which is a worse outcome than losing the text or looping
/// forever, and no narrower option exists without a segmentation crate.
fn split_prefix(text: &str, budget: usize) -> (String, &str) {
  let mut head = String::new();
  let mut head_width = 0usize;
  let mut rest = text;
  for (offset, c) in text.char_indices() {
    let c_width = char_width(c);
    if head_width + c_width > budget {
      rest = &text[offset..];
      break;
    }
    head.push(c);
    head_width += c_width;
    rest = &text[offset + c.len_utf8()..];
  }
  if head.is_empty() {
    // A single character wider than the whole budget still has to be emitted, or the
    // caller would loop forever on it.
    let mut chars = text.chars();
    if let Some(c) = chars.next() {
      head.push(c);
      rest = &text[c.len_utf8()..];
    }
  }
  (head, rest)
}

/// One wrap candidate: a run of spaces, or a run of non-spaces.
#[derive(Debug, Clone, Copy)]
struct Atom<'a> {
  text: &'a str,
  role: Role,
  space: bool,
}

fn atoms(line: &RenderLine) -> Vec<Atom<'_>> {
  let mut atoms = Vec::new();
  for segment in &line.segments {
    let mut offset = 0usize;
    while offset < segment.text.len() {
      let space = segment.text[offset..].starts_with(' ');
      let end = segment.text[offset..]
        .char_indices()
        .find(|(_, c)| (*c == ' ') != space)
        .map(|(i, _)| offset + i)
        .unwrap_or(segment.text.len());
      atoms.push(Atom {
        text: &segment.text[offset..end],
        role: segment.role,
        space,
      });
      offset = end;
    }
  }
  atoms
}

impl fmt::Display for RenderLine {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&self.plain())
  }
}

/// What wrapping may do to decoration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NarrowDecoration {
  /// Keep indentation, and keep word wrapping above the narrow-column floor.
  Keep,
  /// Drop indentation. Used when the prefix itself is competing with content.
  Strip,
}

fn leading_columns(line: &RenderLine) -> usize {
  line
    .segments
    .first()
    .map(|first| first.text.chars().take_while(|c| *c == ' ').count())
    .unwrap_or(0)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn streamed_text_merges_into_one_segment_per_role() {
    let mut line = RenderLine::new();
    line.push("hel", Role::Assistant);
    line.push("lo", Role::Assistant);
    assert_eq!(line.segments.len(), 1);
    assert_eq!(line.plain(), "hello");
    line.push(" · 3 ms", Role::Meta);
    assert_eq!(line.segments.len(), 2);
    assert_eq!(line.plain(), "hello · 3 ms");
  }

  #[test]
  fn operation_argument_and_path_stay_separate_segments() {
    let mut line = RenderLine::new();
    line.push("read", Role::Operation);
    line.push(" ", Role::Muted);
    line.push("crates/pi-rs-tui/src/lib.rs", Role::Path);
    line.push(" --offset=10", Role::Flag);
    assert_eq!(line.segments.len(), 4);
    assert_eq!(line.width(), 4 + 1 + 27 + 12);
    assert_eq!(line.plain(), "read crates/pi-rs-tui/src/lib.rs --offset=10");
  }

  #[test]
  fn paint_distinguishes_roles_and_plain_does_not() {
    let mut line = RenderLine::new();
    line.push("grep", Role::Operation);
    line.push(" needle", Role::Argument);
    assert_eq!(line.plain(), "grep needle");
    let painted = line.render(Palette::colored());
    assert!(painted.contains("\x1b[1;36mgrep\x1b[0m"), "{painted}");
    assert_eq!(line.render(Palette::monochrome()), "grep needle");
  }

  #[test]
  fn wrapping_keeps_roles_on_continuation_lines() {
    let mut line = RenderLine::new();
    line.push("  exec", Role::Operation);
    line.push(" ", Role::Muted);
    line.push("cargo test --workspace --all-targets", Role::Argument);
    let lines = line.wrapped(24, NarrowDecoration::Keep);
    assert!(lines.len() > 1);
    assert!(
      lines
        .iter()
        .any(|l| l.segments.iter().any(|s| s.role == Role::Operation)),
      "{lines:?}"
    );
    assert!(
      lines
        .iter()
        .any(|l| l.segments.iter().any(|s| s.role == Role::Argument)),
      "{lines:?}"
    );
    for line in &lines {
      assert!(
        line.width() <= 24,
        "{} columns: {:?}",
        line.width(),
        line.plain()
      );
    }
  }

  #[test]
  fn a_zero_budget_means_do_not_wrap() {
    // A piped transcript has no column budget, which is not the same as a budget of
    // one column. Conflating them turns every line into one character per line.
    let mut line = RenderLine::new();
    line.push("[answer] ", Role::Muted);
    line.push("one line, written whole", Role::Assistant);
    assert_eq!(line.wrapped(0, NarrowDecoration::Keep).len(), 1);
    assert_eq!(line.wrapped(0, NarrowDecoration::Strip).len(), 1);
  }

  #[test]
  fn wrapping_does_not_glue_two_segments_together() {
    // Wrapping each segment inside its own budget used to lose the space between them
    // whenever that space landed on a break: `[tool failed]` and `exec` came out as
    // `[tool failed]exec`. The separator belongs to whichever line holds both.
    let mut line = RenderLine::new();
    line.push("[tool failed] ", Role::Muted);
    line.push("exec", Role::Operation);
    line.push(" \u{b7} ", Role::Muted);
    line.push("exit 1", Role::Error);
    let lines = line.wrapped(20, NarrowDecoration::Keep);
    let texts: Vec<String> = lines.iter().map(|line| line.plain()).collect();
    for text in &texts {
      assert!(!text.ends_with(' '), "trailing space: {text:?}");
      assert!(!text.starts_with(' '), "leading space: {text:?}");
    }
    assert_eq!(
      texts,
      vec!["[tool failed] exec \u{b7}", "exit 1"],
      "{texts:?}"
    );
  }

  #[test]
  fn a_short_label_does_not_get_a_line_to_itself() {
    // A long body segment after a short label used to flush the label onto its own line
    // and leave most of the first line blank. Filling keeps the reader's columns usable.
    let mut line = RenderLine::new();
    line.push("[answer] ", Role::Muted);
    line.push("one two three four five six seven", Role::Assistant);
    let texts: Vec<String> = line
      .wrapped(20, NarrowDecoration::Keep)
      .iter()
      .map(|line| line.plain())
      .collect();
    assert_eq!(
      texts,
      vec!["[answer] one two", "three four five six", "seven"],
      "{texts:?}"
    );
  }

  #[test]
  fn narrow_width_strips_indent_and_never_panics() {
    let mut line = RenderLine::new();
    line.push("    read", Role::Operation);
    line.push(" some/long/path", Role::Path);
    for width in 1..40 {
      for rendered in line.wrapped(width, NarrowDecoration::Strip) {
        assert!(
          rendered.width() <= width.max(1),
          "{width}: {:?}",
          rendered.plain()
        );
      }
    }
  }

  #[test]
  fn wide_path_hard_splits_without_losing_its_role() {
    let mut line = RenderLine::new();
    line.push(
      "crates/pi-rs-core/src/extremely_long_file_name.rs",
      Role::Path,
    );
    let lines = line.wrapped(20, NarrowDecoration::Strip);
    assert!(lines.len() >= 3);
    assert!(
      lines
        .iter()
        .all(|l| l.segments.iter().all(|s| s.role == Role::Path)),
      "{lines:?}"
    );
  }
}
