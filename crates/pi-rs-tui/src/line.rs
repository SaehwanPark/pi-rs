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
  width::{MIN_COLUMN, display_width, wrap},
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
  /// operation/argument/path distinction on continuation lines. A wide segment is
  /// hard-split at the column budget; the pieces keep their role, which is why a
  /// path that wraps across two lines is still recognisably a path on both.
  pub fn wrapped(&self, width: usize, narrow_decoration: NarrowDecoration) -> Vec<RenderLine> {
    if self.width() <= width {
      return vec![self.clone()];
    }
    let indent = match narrow_decoration {
      NarrowDecoration::Keep if width >= MIN_COLUMN => leading_columns(self),
      _ => 0,
    };
    debug_assert!(indent <= width, "indent was measured from this line");
    let indent = indent.min(width.saturating_sub(1));
    let body_width = (width - indent).max(1);

    let mut lines = Vec::new();
    let mut current = RenderLine::new();
    let mut current_width = 0usize;
    let mut pending_indent = indent > 0;

    for segment in &self.segments {
      // `wrap` already falls back to hard splitting below the narrow-column
      // floor, so the same call serves both decoration modes.
      let pieces = wrap(&segment.text, body_width);
      for piece in pieces {
        let piece_width = display_width(&piece);
        if current_width + piece_width > body_width && !current.is_empty() {
          lines.push(std::mem::take(&mut current));
          current_width = 0;
          pending_indent = indent > 0;
        }
        if pending_indent && indent > 0 {
          current.push(&" ".repeat(indent), Role::Muted);
          current_width += indent;
          pending_indent = false;
        }
        current.push(&piece, segment.role);
        current_width += piece_width;
        if current_width >= body_width {
          lines.push(std::mem::take(&mut current));
          current_width = 0;
          pending_indent = indent > 0;
        }
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
