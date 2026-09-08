//! The buffer a user types into.
//!
//! This module owns text, not keys, not rendering, and not what a submission
//! means. A surface translates terminal events into [`Intent`]s, hands them to
//! [`Editor::apply`], and asks [`Editor::display`] for the rows to draw and the
//! column the caret belongs on. Nobody else is allowed to keep cursor state.
//!
//! Three rules shape it.
//!
//! **Columns are display columns, offsets are characters.** The two are not the
//! same: one Hangul syllable is one character and two columns, and a caret that
//! counts characters while the terminal counts cells walks off the end of
//! perfectly ordinary Korean text. Every place this module speaks of a column
//! means what [`crate::width`] measures; every offset inside a line counts
//! characters, because that is the unit an insertion can land on.
//!
//! **A buffer is lines, and always at least one.** The empty buffer is one empty
//! line, never zero lines, so callers never have to handle "no line to type
//! into" and the cursor invariants below hold unconditionally.
//!
//! **Motion remembers a column, editing does not.** Pressing down-arrow three
//! times through a paragraph of short lines returns to the column the user was
//! on, which is what makes the caret feel parked. Any horizontal edit or move
//! drops that memory, because after an insertion the old column is no longer the
//! column the user is looking at.
//!
//! # Invariants
//!
//! * `lines` is non-empty;
//! * `cursor.line < lines.len()`;
//! * `cursor.column <= chars(lines[cursor.line])`;
//! * no line contains a newline;
//! * `width >= 1`.

use crate::width::{char_width, display_width};

/// A caret position: which line, and how many characters into it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
  /// Zero-based line index.
  pub line: usize,
  /// Characters from the start of that line, never past its length.
  pub column: usize,
}

/// What a surface asked the buffer to do.
///
/// Deliberately semantic rather than terminal-scancode shaped: the same intent
/// reaches the buffer whether it arrived from crossterm, from a paste bracket,
/// or from a test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
  /// Type one character at the caret.
  Insert(char),
  /// Insert pasted text, which may contain newlines.
  Paste(String),
  /// Split the line at the caret.
  InsertNewline,
  /// Remove the character before the caret, or join with the line above.
  Backspace,
  /// Remove the character after the caret, or join with the line below.
  DeleteForward,
  /// Discard from the caret to the start of the line.
  DeleteToLineStart,
  /// Discard from the caret to the end of the line.
  DeleteToLineEnd,
  /// Discard the word before the caret.
  DeleteWordBackward,
  /// One character left, joining with the line above at the edge.
  MoveLeft,
  /// One character right, joining with the line below at the edge.
  MoveRight,
  /// One word left.
  MoveWordLeft,
  /// One word right.
  MoveWordRight,
  /// To the first column of the line, then to the true start if already there.
  MoveLineStart,
  /// Past the last character of the line.
  MoveLineEnd,
  /// One display row up, or older input when already on the first row.
  MoveUp,
  /// One display row down, or newer input when already on the last row.
  MoveDown,
  /// To the very first character of the buffer.
  MoveBufferStart,
  /// Past the very last character of the buffer.
  MoveBufferEnd,
  /// Send what is typed, if anything sendable is.
  Submit,
  /// Give up the current input without sending it.
  Cancel,
  /// A key this buffer has no binding for.
  Noop,
}

/// What applying an [`Intent`] produced.
///
/// Typed rather than a flag, because the caller's next action differs: repaint
/// for [`Outcome::Changed`], hand text to the runtime for
/// [`Outcome::Submit`], and do nothing at all for [`Outcome::Unchanged`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
  /// Nothing moved, including a submit with nothing sendable in it.
  Unchanged,
  /// Text or caret moved; the caller repaints.
  Changed,
  /// The user submitted `text`. The buffer is empty again.
  Submit(String),
  /// The user discarded what was typed. The buffer is empty again.
  Cancelled,
}

/// A multi-line text buffer with a caret and recall of what was sent before.
#[derive(Debug, Clone)]
pub struct Editor {
  lines: Vec<String>,
  cursor: Cursor,
  /// Column vertical motion aims at, `None` once the caret has been placed by
  /// hand or by an edit.
  desired: Option<usize>,
  width: usize,
  history: Vec<String>,
  /// Which history entry is recalled, counting back from the newest.
  recall: Option<usize>,
  /// What was in the buffer when recall started.
  draft: Option<String>,
}

impl Default for Editor {
  fn default() -> Self {
    Self::new()
  }
}

impl Editor {
  /// An empty buffer wide enough for ordinary prose.
  pub fn new() -> Self {
    Self::with_width(80)
  }

  /// An empty buffer that soft-wraps at `width` display columns.
  ///
  /// A width of zero is clamped to one column: a zero-width buffer has no
  /// addressable caret, and a caller that passes a bogus terminal size gets a
  /// usable editor instead of a panic.
  pub fn with_width(width: usize) -> Self {
    Self {
      lines: vec![String::new()],
      cursor: Cursor { line: 0, column: 0 },
      desired: None,
      width: width.max(1),
      history: Vec::new(),
      recall: None,
      draft: None,
    }
  }

  /// Re-measure the buffer, as a resize handler would.
  pub fn set_width(&mut self, width: usize) {
    self.width = width.max(1);
    self.desired = None;
  }

  /// Columns the buffer soft-wraps at.
  pub fn width(&self) -> usize {
    self.width
  }

  /// The whole buffer, lines joined with newlines.
  pub fn text(&self) -> String {
    self.lines.join("\n")
  }

  /// `true` when there is nothing typed.
  pub fn is_empty(&self) -> bool {
    self.lines.len() == 1 && self.lines[0].is_empty()
  }

  /// Where the caret is.
  pub fn cursor(&self) -> Cursor {
    self.cursor
  }

  /// What has been submitted, oldest first.
  pub fn history(&self) -> &[String] {
    &self.history
  }
  /// Apply one intent.
  pub fn apply(&mut self, intent: Intent) -> Outcome {
    match intent {
      Intent::Insert(c) => {
        self.recall_reset_on_edit();
        self.insert_str(&c.to_string());
        Outcome::Changed
      }
      Intent::Paste(text) => {
        if text.is_empty() {
          return Outcome::Unchanged;
        }
        self.recall_reset_on_edit();
        self.insert_str(&text);
        Outcome::Changed
      }
      Intent::InsertNewline => {
        self.recall_reset_on_edit();
        self.split_line();
        Outcome::Changed
      }
      Intent::Backspace => {
        self.recall_reset_on_edit();
        if self.delete_backward() {
          Outcome::Changed
        } else {
          Outcome::Unchanged
        }
      }
      Intent::DeleteForward => {
        self.recall_reset_on_edit();
        if self.delete_ahead() {
          Outcome::Changed
        } else {
          Outcome::Unchanged
        }
      }
      Intent::DeleteToLineStart => {
        self.recall_reset_on_edit();
        if self.delete_to_start() {
          Outcome::Changed
        } else {
          Outcome::Unchanged
        }
      }
      Intent::DeleteToLineEnd => {
        self.recall_reset_on_edit();
        if self.delete_to_end() {
          Outcome::Changed
        } else {
          Outcome::Unchanged
        }
      }
      Intent::DeleteWordBackward => {
        self.recall_reset_on_edit();
        if self.delete_word() {
          Outcome::Changed
        } else {
          Outcome::Unchanged
        }
      }
      Intent::MoveLeft => self.move_left(),
      Intent::MoveRight => self.move_right(),
      Intent::MoveWordLeft => self.move_word(-1),
      Intent::MoveWordRight => self.move_word(1),
      Intent::MoveLineStart => self.move_line_start(),
      Intent::MoveLineEnd => self.move_line_end(),
      Intent::MoveUp => self.move_row(-1),
      Intent::MoveDown => self.move_row(1),
      Intent::MoveBufferStart => {
        self.desired = None;
        if self.cursor == (Cursor { line: 0, column: 0 }) {
          Outcome::Unchanged
        } else {
          self.cursor = Cursor { line: 0, column: 0 };
          Outcome::Changed
        }
      }
      Intent::MoveBufferEnd => {
        self.desired = None;
        let last = self.lines.len() - 1;
        let end = Cursor {
          line: last,
          column: char_count(&self.lines[last]),
        };
        if self.cursor == end {
          Outcome::Unchanged
        } else {
          self.cursor = end;
          Outcome::Changed
        }
      }
      Intent::Submit => {
        let text = self.text().trim().to_string();
        if text.is_empty() {
          return Outcome::Unchanged;
        }
        if self.history.last().is_none_or(|last| last != &text) {
          self.history.push(text.clone());
        }
        self.reset();
        Outcome::Submit(text)
      }
      Intent::Cancel => {
        if self.is_empty() {
          return Outcome::Unchanged;
        }
        self.reset();
        Outcome::Cancelled
      }
      Intent::Noop => Outcome::Unchanged,
    }
  }

  /// Discard the buffer and any recall, keeping history.
  fn reset(&mut self) {
    self.lines = vec![String::new()];
    self.cursor = Cursor { line: 0, column: 0 };
    self.desired = None;
    self.recall = None;
    self.draft = None;
  }

  /// Typing or editing ends a recall: the recalled text is now the user's own.
  fn recall_reset_on_edit(&mut self) {
    self.desired = None;
    self.recall = None;
    self.draft = None;
  }

  /// Insert text, treating newlines as line breaks and ignoring carriage returns
  /// so a Windows paste does not leave stray control characters in the buffer.
  fn insert_str(&mut self, text: &str) {
    let text = text.replace('\r', "");
    let mut parts = text.split('\n');
    let first = parts.next().unwrap_or_default();
    self.splice_first(first);
    for part in parts {
      self.split_line();
      self.splice_first(part);
    }
  }

  /// Insert `text` at the caret inside the current line, splitting it.
  fn splice_first(&mut self, text: &str) {
    if text.is_empty() {
      return;
    }
    let line = &mut self.lines[self.cursor.line];
    let byte = byte_offset(line, self.cursor.column);
    line.insert_str(byte, text);
    self.cursor.column += text.chars().count();
  }

  /// Break the current line at the caret and open the next one.
  fn split_line(&mut self) {
    let line = &self.lines[self.cursor.line];
    let byte = byte_offset(line, self.cursor.column);
    let tail = line[byte..].to_string();
    self.lines[self.cursor.line].truncate(byte);
    self.lines.insert(self.cursor.line + 1, tail);
    self.cursor.line += 1;
    self.cursor.column = 0;
  }

  /// Remove the character before the caret, joining lines at the boundary.
  fn delete_backward(&mut self) -> bool {
    if self.cursor.column > 0 {
      let line = &self.lines[self.cursor.line];
      let byte = byte_offset(line, self.cursor.column);
      let previous = prev_boundary(line, byte);
      self.lines[self.cursor.line].remove(previous);
      self.cursor.column -= 1;
      return true;
    }
    if self.cursor.line == 0 {
      return false;
    }
    let join_at = char_count(&self.lines[self.cursor.line - 1]);
    let tail = self.lines.remove(self.cursor.line);
    self.cursor.line -= 1;
    self.cursor.column = join_at;
    let target = self.cursor.line;
    self.lines[target].push_str(&tail);
    true
  }

  /// Remove the character after the caret, pulling the next line up at the edge.
  fn delete_ahead(&mut self) -> bool {
    let count = char_count(&self.lines[self.cursor.line]);
    if self.cursor.column < count {
      let line = &self.lines[self.cursor.line];
      let byte = byte_offset(line, self.cursor.column);
      self.lines[self.cursor.line].remove(byte);
      return true;
    }
    if self.cursor.line + 1 >= self.lines.len() {
      return false;
    }
    let tail = self.lines.remove(self.cursor.line + 1);
    let target = self.cursor.line;
    self.lines[target].push_str(&tail);
    true
  }

  /// Remove from the caret back to the start of the line.
  fn delete_to_start(&mut self) -> bool {
    if self.cursor.column == 0 {
      return false;
    }
    let line = &self.lines[self.cursor.line];
    let byte = byte_offset(line, self.cursor.column);
    self.lines[self.cursor.line].replace_range(..byte, "");
    self.cursor.column = 0;
    true
  }

  /// Remove from the caret to the end of the line.
  fn delete_to_end(&mut self) -> bool {
    let line = &self.lines[self.cursor.line];
    let count = char_count(line);
    if self.cursor.column >= count {
      return false;
    }
    let byte = byte_offset(line, self.cursor.column);
    self.lines[self.cursor.line].truncate(byte);
    true
  }

  /// Remove the word before the caret, keeping the whitespace in front of it.
  fn delete_word(&mut self) -> bool {
    if self.cursor.column == 0 {
      return self.delete_backward();
    }
    let line = &self.lines[self.cursor.line];
    let target = word_boundary(line, self.cursor.column);
    let from = byte_offset(line, target);
    let to = byte_offset(line, self.cursor.column);
    self.lines[self.cursor.line].replace_range(from..to, "");
    self.cursor.column = target;
    true
  }

  fn move_left(&mut self) -> Outcome {
    self.desired = None;
    if self.cursor.column > 0 {
      self.cursor.column -= 1;
      Outcome::Changed
    } else if self.cursor.line > 0 {
      self.cursor.line -= 1;
      self.cursor.column = char_count(&self.lines[self.cursor.line]);
      Outcome::Changed
    } else {
      Outcome::Unchanged
    }
  }

  fn move_right(&mut self) -> Outcome {
    self.desired = None;
    let count = char_count(&self.lines[self.cursor.line]);
    if self.cursor.column < count {
      self.cursor.column += 1;
      Outcome::Changed
    } else if self.cursor.line + 1 < self.lines.len() {
      self.cursor.line += 1;
      self.cursor.column = 0;
      Outcome::Changed
    } else {
      Outcome::Unchanged
    }
  }

  /// Step a word at a time. `direction` is `-1` or `1`.
  fn move_word(&mut self, direction: i32) -> Outcome {
    self.desired = None;
    let before = self.cursor;
    loop {
      // Cross a line edge first, then skip whitespace, then consume the word.
      let at_edge = if direction < 0 {
        self.cursor.column == 0 && self.cursor.line > 0
      } else {
        self.cursor.column == char_count(&self.lines[self.cursor.line])
          && self.cursor.line + 1 < self.lines.len()
      };
      if at_edge {
        if direction < 0 {
          self.cursor.line -= 1;
          self.cursor.column = char_count(&self.lines[self.cursor.line]);
        } else {
          self.cursor.line += 1;
          self.cursor.column = 0;
        }
        continue;
      }
      let line = &self.lines[self.cursor.line];
      let next = if direction < 0 {
        (self.cursor.column > 0).then(|| word_boundary(line, self.cursor.column))
      } else {
        word_end(line, self.cursor.column)
      };
      match next {
        Some(column) if column != self.cursor.column => {
          self.cursor.column = column;
          break;
        }
        _ => break,
      }
    }
    if self.cursor == before {
      Outcome::Unchanged
    } else {
      Outcome::Changed
    }
  }

  fn move_line_start(&mut self) -> Outcome {
    self.desired = None;
    let skip = indent_width(&self.lines[self.cursor.line]);
    let target = if self.cursor.column == 0 {
      char_count(&self.lines[self.cursor.line])
    } else if self.cursor.column <= skip {
      0
    } else {
      skip
    };
    if self.cursor.column == target {
      return Outcome::Unchanged;
    }
    self.cursor.column = target;
    Outcome::Changed
  }

  fn move_line_end(&mut self) -> Outcome {
    self.desired = None;
    let count = char_count(&self.lines[self.cursor.line]);
    if self.cursor.column == count {
      Outcome::Unchanged
    } else {
      self.cursor.column = count;
      Outcome::Changed
    }
  }

  /// One display row up or down, keeping the column motion aims at.
  ///
  /// At the top edge vertical motion recalls older input, and at the bottom edge
  /// it returns toward what was typed last. One key does both because the
  /// alternative is a mode the user has to be in, and a mode is a state a caret
  /// should never have to explain.
  fn move_row(&mut self, direction: i32) -> Outcome {
    let rows = self.buffer_rows();
    let (index, row_column) = caret_row(&rows, &self.lines, self.cursor);
    let target = if direction < 0 {
      index.checked_sub(1)
    } else if index + 1 < rows.len() {
      Some(index + 1)
    } else {
      None
    };
    let Some(target) = target else {
      return self.recall(direction);
    };
    let wanted = self.desired.unwrap_or(row_column);
    let row = rows[target];
    let column = column_at(&self.lines[row.line], row.start, row.end, wanted);
    self.desired = Some(wanted);
    self.cursor = Cursor {
      line: row.line,
      column,
    };
    Outcome::Changed
  }

  /// Recall input by position: `-1` older, `1` newer.
  ///
  /// The text being replaced when recall starts is kept as a draft, so pressing
  /// down-arrow back past the newest entry gives it back rather than losing it.
  fn recall(&mut self, direction: i32) -> Outcome {
    if direction < 0 {
      let next = self.recall.map_or(0, |recalled| recalled + 1);
      if next >= self.history.len() {
        return Outcome::Unchanged;
      }
      if self.recall.is_none() {
        self.draft = Some(self.text());
      }
      self.recall = Some(next);
    } else {
      let Some(current) = self.recall else {
        return Outcome::Unchanged;
      };
      if current == 0 {
        let draft = self.draft.take().unwrap_or_default();
        self.recall = None;
        self.load(&draft);
        return Outcome::Changed;
      }
      self.recall = Some(current - 1);
    }
    let recalled = self.recall.unwrap_or(0);
    let text = self.history[self.history.len() - 1 - recalled].clone();
    self.load(&text);
    Outcome::Changed
  }

  /// Replace the buffer with `text`, caret to the end.
  fn load(&mut self, text: &str) {
    self.lines = text.split('\n').map(str::to_string).collect();
    self.cursor = Cursor {
      line: self.lines.len() - 1,
      column: char_count(self.lines.last().unwrap_or(&String::new())),
    };
    self.desired = None;
  }

  /// Rows to print, and where the caret lands among them.
  pub fn display(&self, prefix: &str) -> Layout {
    let pad = " ".repeat(display_width(prefix));
    let mut rows: Vec<String> = Vec::new();
    let mut caret = Cursor { line: 0, column: 0 };
    let mut seen = false;
    for (line, text) in self.lines.iter().enumerate() {
      let spans = line_rows(text, self.width);
      let last_index = spans.len() - 1;
      for (index, (start, end)) in spans.into_iter().enumerate() {
        let content: String = text.chars().skip(start).take(end - start).collect();
        let first_row = line == 0 && index == 0;
        rows.push(format!(
          "{}{content}",
          if first_row { prefix } else { pad.as_str() }
        ));
        let on_this_row = self.cursor.line == line
          && self.cursor.column >= start
          && (self.cursor.column < end || index == last_index);
        if on_this_row && !seen {
          seen = true;
          let inside: String = text
            .chars()
            .skip(start)
            .take(self.cursor.column - start)
            .collect();
          caret = Cursor {
            line: rows.len() - 1,
            column: if first_row {
              display_width(prefix)
            } else {
              display_width(&pad)
            } + display_width(&inside),
          };
        }
      }
    }
    Layout {
      rows,
      cursor: caret,
    }
  }

  /// Every display row of the buffer, in print order.
  fn buffer_rows(&self) -> Vec<Row> {
    let mut rows = Vec::new();
    for (line, text) in self.lines.iter().enumerate() {
      for (start, end) in line_rows(text, self.width) {
        rows.push(Row { line, start, end });
      }
    }
    rows
  }
}

/// The buffer as a surface draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
  /// Rows to print top to bottom, each carrying the prompt prefix or its indent.
  pub rows: Vec<String>,
  /// Caret as a row index into `rows` and a display column within it, prefix
  /// included, so a surface can place a hardware cursor without arithmetic.
  pub cursor: Cursor,
}

/// One display row of one buffer line, addressed in characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Row {
  line: usize,
  start: usize,
  end: usize,
}

/// Characters in `text`.
fn char_count(text: &str) -> usize {
  text.chars().count()
}

/// Byte offset of character `column`, clamped to the end of the line.
fn byte_offset(line: &str, column: usize) -> usize {
  line
    .chars()
    .take(column)
    .map(char::len_utf8)
    .sum::<usize>()
    .min(line.len())
}

/// Start of the character that ends just before `byte`.
fn prev_boundary(text: &str, byte: usize) -> usize {
  if byte == 0 {
    return 0;
  }
  let mut index = byte - 1;
  while index > 0 && !text.is_char_boundary(index) {
    index -= 1;
  }
  index
}

/// Word characters are the ones a word would be made of when typed.
fn is_word_char(c: char) -> bool {
  c.is_alphanumeric() || c == '_'
}

/// Start of the word before `column`, keeping the whitespace in front of it.
fn word_boundary(line: &str, column: usize) -> usize {
  let chars: Vec<char> = line.chars().take(column).collect();
  let mut index = chars.len();
  while index > 0 && !is_word_char(chars[index - 1]) {
    index -= 1;
  }
  while index > 0 && is_word_char(chars[index - 1]) {
    index -= 1;
  }
  index
}

/// End of the word after `column`, or `None` when the line ends here.
fn word_end(line: &str, column: usize) -> Option<usize> {
  let chars: Vec<char> = line.chars().collect();
  let mut index = column;
  while index < chars.len() && !is_word_char(chars[index]) {
    index += 1;
  }
  while index < chars.len() && is_word_char(chars[index]) {
    index += 1;
  }
  (index > column).then_some(index)
}

/// Leading whitespace, counted in characters.
fn indent_width(line: &str) -> usize {
  line.chars().take_while(|c| c.is_whitespace()).count()
}

/// Rows of one line, split by display columns.
///
/// Hard splits, not word wrapping. `crate::width::wrap` word-wraps, which is right
/// for a transcript the user reads and wrong for a buffer the user navigates: a
/// caret has to be addressable at every column, and arrow keys that move by
/// character cannot be made to agree with rows that break between words. What is
/// left in common is the measurement, so a wide glyph moves the break exactly as
/// far as it moves the text.
/// A break is taken between clusters, never inside one ([[`continues_cluster`]]):
/// the cluster a row would have broken on is spent whole on the row it started on,
/// so a joined emoji, a marked letter, a keycap, or a flag pair is never left with
/// half of itself on each side of a row end. Rows are never merged and every row
/// stays non-empty, so the one-segment-per-row assumption under the cursor walk and
/// the erase count holds. A cluster wider than the row cannot be cut, so that row
/// goes over — the same concession a double-width glyph already makes at width 1.
fn line_rows(line: &str, width: usize) -> Vec<(usize, usize)> {
  let width = width.max(1);
  let chars: Vec<char> = line.chars().collect();
  let mut rows = Vec::new();
  let mut start = 0usize;
  let mut column = 0usize;
  let mut index = 0usize;
  while index < chars.len() {
    // The cluster is the widest span from `index` whose scalars may not be
    // separated, so the only boundaries this loop can break at are the ones a
    // cluster allows.
    let mut end = index + 1;
    while end < chars.len() && continues_cluster(Some(chars[end - 1]), chars[end]) {
      end += 1;
    }
    let columns: usize = chars[index..end].iter().copied().map(char_width).sum();
    if column + columns > width && index > start {
      rows.push((start, index));
      start = index;
      column = 0;
    }
    column += columns;
    index = end;
  }
  rows.push((start, chars.len()));
  rows
}

/// `true` when `next` is a cluster tail: it belongs to the cluster `previous`
/// opens, so a row may not begin there.
///
/// The rule is deliberately narrow. `unicode-segmentation` is not a dependency and
/// must not become one for this, and hand-writing UAX #29 clustering inside a wrap
/// would be more code than the wrap it fixes. What is covered, and all this
/// claims: a joiner U+200D and the scalar it joins, on both sides of the joiner;
/// the non-joiner U+200C; the variation selectors U+FE0E and U+FE0F; the combining
/// enclosing keycap U+20E3; combining diacritical marks U+0300..=U+036F; and the
/// second regional indicator of a flag pair, two scalars a terminal draws as one
/// glyph. That is emoji sequences, combining marks and keycaps.
///
/// What is not: this is not Unicode grapheme parity. Indic vowel signs, Thai
/// combining marks, and Hangul jamo sequences can still be broken apart. A missed
/// cluster at least measures columns, so the width checks still see it; a missed
/// joiner measures nothing, which is why the joiner is on the list.
fn continues_cluster(previous: Option<char>, next: char) -> bool {
  // A joiner never ends a row and what it joins never starts one.
  let follows_a_joiner = matches!(previous, Some('\u{200d}'));
  // Only the halves of a flag are attached to each other: what follows a lone
  // regional indicator, when that is not another indicator, opens a new cluster.
  let joins_a_flag_pair = match previous {
    Some(previous) if is_regional_indicator(previous) => is_regional_indicator(next),
    _ => false,
  };
  follows_a_joiner
    || joins_a_flag_pair
    || matches!(
      next,
      // Non-joiner, joiner, enclosing keycap, text and emoji variation selectors.
      '\u{200c}' | '\u{200d}' | '\u{20e3}' | '\u{fe0e}' | '\u{fe0f}'
    )
    || matches!(next as u32, 0x0300..=0x036f)
}

/// `true` for a regional indicator, the half of a flag pair: U+1F1E6..=U+1F1FF.
fn is_regional_indicator(c: char) -> bool {
  matches!(c as u32, 0x1f1e6..=0x1f1ff)
}

/// Which display row the caret sits on, and how far into it.
fn caret_row(rows: &[Row], lines: &[String], cursor: Cursor) -> (usize, usize) {
  for (index, row) in rows.iter().enumerate() {
    if row.line != cursor.line {
      continue;
    }
    let last_of_line = rows.get(index + 1).is_none_or(|next| next.line != row.line);
    if cursor.column >= row.start && (cursor.column < row.end || last_of_line) {
      let inside: String = lines[row.line]
        .chars()
        .skip(row.start)
        .take(cursor.column - row.start)
        .collect();
      return (index, display_width(&inside));
    }
  }
  (0, 0)
}

/// Character index in `start..end` whose cell holds display column `wanted`.
fn column_at(line: &str, start: usize, end: usize, wanted: usize) -> usize {
  let mut column = 0usize;
  for (offset, c) in line.chars().skip(start).take(end - start).enumerate() {
    let w = char_width(c);
    if column + w > wanted {
      return start + offset;
    }
    column += w;
    if column >= wanted {
      return start + offset + 1;
    }
  }
  end
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Type a whole string, as the surface would when the user does.
  fn typed(text: &str) -> Editor {
    let mut editor = Editor::new();
    for c in text.chars() {
      editor.apply(Intent::Insert(c));
    }
    editor
  }

  fn text(editor: &Editor) -> String {
    editor.text()
  }

  #[test]
  fn typing_inserts_at_the_caret() {
    let mut editor = typed("hello");
    editor.apply(Intent::MoveLeft);
    editor.apply(Intent::MoveLeft);
    assert_eq!(editor.cursor().column, 3);
    editor.apply(Intent::Insert('X'));
    assert_eq!(text(&editor), "helXlo");
  }

  #[test]
  fn a_newline_splits_the_line_and_the_text_still_reads_as_one() {
    let mut editor = typed("ab");
    editor.apply(Intent::InsertNewline);
    editor.apply(Intent::Insert('c'));
    assert_eq!(text(&editor), "ab\nc");
    assert_eq!(editor.cursor(), Cursor { line: 1, column: 1 });
  }

  #[test]
  fn backspace_at_the_start_joins_the_lines() {
    // "ab\ncd" arrives as a paste, so the caret is already at the end.
    let mut editor = {
      let mut e = Editor::new();
      e.apply(Intent::Paste("ab\ncd".to_string()));
      e.apply(Intent::MoveLeft);
      e.apply(Intent::MoveLeft);
      e
    };
    assert_eq!(editor.cursor(), Cursor { line: 1, column: 0 });
    assert_eq!(editor.apply(Intent::Backspace), Outcome::Changed);
    assert_eq!(text(&editor), "abcd");
    assert_eq!(editor.cursor(), Cursor { line: 0, column: 2 });
  }

  #[test]
  fn backspace_on_untouched_input_does_nothing() {
    let mut editor = Editor::new();
    assert_eq!(editor.apply(Intent::Backspace), Outcome::Unchanged);
    assert!(editor.is_empty());
  }

  #[test]
  fn backspace_removes_a_whole_character_not_a_byte() {
    let mut editor = typed("한");
    editor.apply(Intent::Backspace);
    assert!(editor.is_empty());
    // And the buffer is still typed into, not left mid-codepoint.
    editor.apply(Intent::Insert('x'));
    assert_eq!(text(&editor), "x");
  }

  #[test]
  fn delete_forward_pulls_the_next_line_up() {
    let mut editor = Editor::new();
    editor.apply(Intent::Paste("ab\ncd".to_string()));
    editor.apply(Intent::MoveLineStart);
    editor.apply(Intent::MoveLeft);
    assert_eq!(editor.apply(Intent::DeleteForward), Outcome::Changed);
    assert_eq!(text(&editor), "abcd");
    editor.apply(Intent::MoveBufferEnd);
    assert_eq!(editor.apply(Intent::DeleteForward), Outcome::Unchanged);
  }

  #[test]
  fn a_paste_with_newlines_becomes_lines() {
    let mut editor = Editor::new();
    editor.apply(Intent::Paste("let\n  x = 1\n".to_string()));
    assert_eq!(text(&editor), "let\n  x = 1\n");
    assert_eq!(editor.lines.len(), 3);
    // The caret sits where typing would continue: the empty final line.
    assert_eq!(editor.cursor(), Cursor { line: 2, column: 0 });
  }

  #[test]
  fn a_paste_carries_no_carriage_returns() {
    let mut editor = Editor::new();
    editor.apply(Intent::Paste("a\r\nb".to_string()));
    assert_eq!(text(&editor), "a\nb");
  }

  #[test]
  fn an_empty_paste_changes_nothing() {
    let mut editor = typed("keep");
    assert_eq!(
      editor.apply(Intent::Paste(String::new())),
      Outcome::Unchanged
    );
    assert_eq!(text(&editor), "keep");
  }

  #[test]
  fn no_line_ever_holds_a_newline() {
    let mut editor = Editor::new();
    editor.apply(Intent::Paste("a\n\n\nb".to_string()));
    assert_eq!(editor.lines.len(), 4);
    assert!(editor.lines.iter().all(|line| !line.contains('\n')));
  }

  #[test]
  fn word_motion_steps_words_not_characters() {
    let mut editor = typed("select the file\nnext");
    editor.apply(Intent::MoveBufferStart);
    for expected in [6, 10, 15] {
      assert_eq!(editor.apply(Intent::MoveWordRight), Outcome::Changed);
      assert_eq!(editor.cursor().column, expected);
      assert_eq!(editor.cursor().line, 0);
    }
    // Past the end of the first line, one word step crosses the edge and takes
    // the next word, because a step that only crossed the edge would strand the
    // caret on whitespace.
    assert_eq!(editor.apply(Intent::MoveWordRight), Outcome::Changed);
    assert_eq!(editor.cursor(), Cursor { line: 1, column: 4 });
    assert_eq!(editor.apply(Intent::MoveWordRight), Outcome::Unchanged);
  }

  #[test]
  fn word_motion_left_stops_at_the_indentation() {
    let mut editor = typed("  fn main");
    assert_eq!(editor.apply(Intent::MoveWordLeft), Outcome::Changed);
    assert_eq!(editor.cursor().column, 5);
    assert_eq!(editor.apply(Intent::MoveWordLeft), Outcome::Changed);
    assert_eq!(editor.cursor().column, 2);
    assert_eq!(editor.apply(Intent::MoveWordLeft), Outcome::Changed);
    assert_eq!(editor.cursor().column, 0);
    assert_eq!(editor.apply(Intent::MoveWordLeft), Outcome::Unchanged);
  }

  #[test]
  fn delete_word_takes_the_word_and_leaves_the_space() {
    let mut editor = typed("let result");
    assert_eq!(editor.apply(Intent::DeleteWordBackward), Outcome::Changed);
    assert_eq!(text(&editor), "let ");
    assert_eq!(editor.cursor().column, 4);
    assert_eq!(editor.apply(Intent::DeleteWordBackward), Outcome::Changed);
    assert_eq!(text(&editor), "");
    assert_eq!(editor.apply(Intent::DeleteWordBackward), Outcome::Unchanged);
  }

  #[test]
  fn home_cycles_indent_start_and_line_end() {
    let mut editor = typed("    let x");
    assert_eq!(editor.cursor().column, 9);
    editor.apply(Intent::MoveLineStart);
    assert_eq!(editor.cursor().column, 4);
    editor.apply(Intent::MoveLineStart);
    assert_eq!(editor.cursor().column, 0);
    editor.apply(Intent::MoveLineStart);
    assert_eq!(editor.cursor().column, 9);
  }

  #[test]
  fn vertical_motion_parks_the_caret_column() {
    let mut editor = Editor::with_width(10);
    editor.apply(Intent::Paste(
      "aaaaaaaaaa\nbb\ncccccccccccccccccccc".to_string(),
    ));
    editor.apply(Intent::MoveBufferStart);
    for _ in 0..3 {
      editor.apply(Intent::MoveRight);
    }
    assert_eq!(editor.cursor(), Cursor { line: 0, column: 3 });
    editor.apply(Intent::MoveDown);
    // The short middle line cannot offer column three, so the caret goes to its
    // end while the aimed-at column survives.
    assert_eq!(editor.cursor(), Cursor { line: 1, column: 2 });
    editor.apply(Intent::MoveDown);
    assert_eq!(editor.cursor(), Cursor { line: 2, column: 3 });
    editor.apply(Intent::MoveUp);
    // One row up is the short line again, so the caret goes to its end. The
    // aimed-at column survives, which is what the next step proves.
    assert_eq!(editor.cursor(), Cursor { line: 1, column: 2 });
    editor.apply(Intent::MoveUp);
    assert_eq!(editor.cursor(), Cursor { line: 0, column: 3 });
  }

  #[test]
  fn vertical_motion_counts_display_rows_not_lines() {
    let mut editor = typed("abcdefghijklmno");
    editor.set_width(10);
    editor.apply(Intent::MoveBufferStart);
    for _ in 0..3 {
      editor.apply(Intent::MoveRight);
    }
    assert_eq!(editor.apply(Intent::MoveDown), Outcome::Changed);
    assert_eq!(
      editor.cursor(),
      Cursor {
        line: 0,
        column: 13
      }
    );
    assert_eq!(editor.apply(Intent::MoveDown), Outcome::Unchanged);
  }

  #[test]
  fn wide_characters_move_the_caret_two_columns() {
    let editor = typed("한글");
    let layout = editor.display("");
    assert_eq!(layout.cursor.column, 4);
    assert_eq!(layout.rows, vec!["한글".to_string()]);
  }

  #[test]
  fn display_wraps_and_indents_continuation_rows() {
    let mut editor = typed("abcdefghijklm");
    editor.set_width(12);
    editor.apply(Intent::MoveLeft);
    let layout = editor.display("> ");
    assert_eq!(
      layout.rows,
      vec!["> abcdefghijkl".to_string(), "  m".to_string()]
    );
    // The caret after the twelfth character sits at the start of the second row,
    // which is where the terminal would show it.
    assert_eq!(layout.cursor, Cursor { line: 1, column: 2 });
  }

  #[test]
  fn display_reports_the_caret_in_printed_columns() {
    let mut editor = typed("가나다");
    editor.set_width(20);
    editor.apply(Intent::MoveLineStart);
    editor.apply(Intent::MoveRight);
    let layout = editor.display("> ");
    // One wide character past a two-column prefix.
    assert_eq!(layout.cursor, Cursor { line: 0, column: 4 });
  }

  #[test]
  fn an_empty_buffer_still_draws_one_row() {
    let editor = Editor::new();
    let layout = editor.display("> ");
    assert_eq!(layout.rows, vec!["> ".to_string()]);
    assert_eq!(layout.cursor, Cursor { line: 0, column: 2 });
  }

  #[test]
  fn a_narrow_terminal_still_has_an_addressable_caret() {
    let mut editor = typed("abcde");
    editor.set_width(0);
    assert_eq!(editor.width(), 1);
    let layout = editor.display("");
    assert_eq!(layout.rows.len(), 5);
    editor.apply(Intent::MoveBufferStart);
    assert_eq!(editor.apply(Intent::MoveDown), Outcome::Changed);
    assert_eq!(editor.cursor().column, 1);
  }

  #[test]
  fn submit_hands_over_text_and_clears_the_buffer() {
    let mut editor = typed("  fix the build\n");
    assert_eq!(
      editor.apply(Intent::Submit),
      Outcome::Submit("fix the build".to_string())
    );
    assert!(editor.is_empty());
    assert_eq!(editor.cursor(), Cursor { line: 0, column: 0 });
    assert_eq!(editor.history(), ["fix the build".to_string()]);
  }

  #[test]
  fn whitespace_is_not_a_submission() {
    let mut editor = typed("   \n");
    assert_eq!(editor.apply(Intent::Submit), Outcome::Unchanged);
    // What was typed survives: a prompt of only whitespace is the user's to fix,
    // not the editor's to eat.
    assert_eq!(text(&editor), "   \n");
    assert!(editor.history().is_empty());
  }

  #[test]
  fn the_same_line_is_not_recorded_twice() {
    let mut editor = typed("make it pass");
    editor.apply(Intent::Submit);
    editor.apply(Intent::Paste("  make it pass  ".to_string()));
    editor.apply(Intent::Submit);
    assert_eq!(editor.history().len(), 1);
  }

  #[test]
  fn up_recalls_older_input_and_down_gives_the_draft_back() {
    let mut editor = Editor::new();
    editor.apply(Intent::Paste("first".to_string()));
    editor.apply(Intent::Submit);
    editor.apply(Intent::Paste("second".to_string()));
    editor.apply(Intent::Submit);
    editor.apply(Intent::Paste("par".to_string()));
    assert_eq!(editor.apply(Intent::MoveUp), Outcome::Changed);
    assert_eq!(text(&editor), "second");
    assert_eq!(editor.apply(Intent::MoveUp), Outcome::Changed);
    assert_eq!(text(&editor), "first");
    assert_eq!(editor.apply(Intent::MoveUp), Outcome::Unchanged);
    editor.apply(Intent::MoveDown);
    editor.apply(Intent::MoveDown);
    assert_eq!(text(&editor), "par");
    assert_eq!(editor.apply(Intent::MoveDown), Outcome::Unchanged);
  }

  #[test]
  fn editing_a_recalled_entry_ends_the_recall() {
    let mut editor = Editor::new();
    editor.apply(Intent::Paste("older".to_string()));
    editor.apply(Intent::Submit);
    editor.apply(Intent::MoveUp);
    assert_eq!(text(&editor), "older");
    editor.apply(Intent::Insert('!'));
    assert_eq!(text(&editor), "older!");
    // The draft was thrown away by the edit, so there is nothing to return to.
    assert_eq!(editor.apply(Intent::MoveDown), Outcome::Unchanged);
  }

  #[test]
  fn recall_only_starts_at_the_first_row() {
    let mut editor = Editor::new();
    editor.apply(Intent::Paste("remember".to_string()));
    editor.apply(Intent::Submit);
    editor.apply(Intent::Paste("a\nb\nc".to_string()));
    editor.apply(Intent::MoveBufferEnd);
    assert_eq!(editor.apply(Intent::MoveUp), Outcome::Changed);
    assert_eq!(text(&editor), "a\nb\nc");
    assert_eq!(editor.cursor(), Cursor { line: 1, column: 1 });
  }

  #[test]
  fn cancel_discards_the_buffer_and_keeps_history() {
    let mut editor = Editor::new();
    editor.apply(Intent::Paste("sent".to_string()));
    editor.apply(Intent::Submit);
    editor.apply(Intent::Paste("dropped".to_string()));
    assert_eq!(editor.apply(Intent::Cancel), Outcome::Cancelled);
    assert!(editor.is_empty());
    assert_eq!(editor.history(), ["sent".to_string()]);
  }

  #[test]
  fn cancel_with_nothing_typed_is_a_no_op() {
    // The surface tells "abandon this input" apart from "there is nothing to
    // abandon", which is how one key can also mean quit.
    let mut editor = Editor::new();
    assert_eq!(editor.apply(Intent::Cancel), Outcome::Unchanged);
  }

  #[test]
  fn the_buffer_never_runs_out_of_lines() {
    let mut editor = typed("a\n\nb");
    for _ in 0..8 {
      editor.apply(Intent::Backspace);
    }
    assert!(editor.is_empty());
    assert_eq!(editor.lines.len(), 1);
    assert_eq!(editor.cursor(), Cursor { line: 0, column: 0 });
    editor.apply(Intent::Insert('z'));
    assert_eq!(text(&editor), "z");
  }

  #[test]
  fn the_cursor_never_leaves_its_line() {
    let mut editor = typed("가나다\nab");
    for _ in 0..20 {
      editor.apply(Intent::MoveRight);
      let Cursor { line, column } = editor.cursor();
      assert!(column <= editor.lines[line].chars().count());
    }
    for _ in 0..20 {
      editor.apply(Intent::MoveLeft);
      assert!(editor.cursor().column <= editor.lines[editor.cursor().line].chars().count());
    }
  }

  #[test]
  fn rows_split_a_line_without_losing_or_reordering_text() {
    // The editor hard-splits where the transcript word-wraps, so the guarantee
    // that matters here is weaker and stronger at once: rows may break a word,
    // they may never break the text, and each row has to fit what it holds.
    for text in ["", "abc", "a b", "가나다", "x 가", "mixed 가나다 long"] {
      for width in [1usize, 2, 3, 7, 40] {
        let spans = line_rows(text, width);
        let joined: String = spans
          .iter()
          .map(|(start, end)| {
            text
              .chars()
              .skip(*start)
              .take(end - start)
              .collect::<String>()
          })
          .collect();
        assert_eq!(joined, text, "width {width} lost text in {text:?}");
        assert_eq!(spans.first().map_or(1, |span| span.0), 0);
        assert_eq!(spans.last().map_or(0, |span| span.1), text.chars().count());
        for pair in spans.windows(2) {
          assert_eq!(pair[0].1, pair[1].0, "width {width} gapped {text:?}");
        }
        for (start, end) in &spans {
          let columns: usize = text
            .chars()
            .skip(*start)
            .take(end - start)
            .map(char_width)
            .sum();
          assert!(
            columns <= width || end - start == 1,
            "width {width} row {start}..{end} of {text:?} is {columns} columns"
          );
        }
      }
    }
  }

  #[test]
  fn an_import_style_buffer_survives_a_whole_editing_pass() {
    // Pasted, edited across lines, recalled, and submitted: the sequence a real
    // session produces, run end to end.
    let mut editor = Editor::with_width(20);
    editor.apply(Intent::Paste("run the tests\nthen commit".to_string()));
    editor.apply(Intent::MoveBufferStart);
    editor.apply(Intent::MoveWordRight);
    editor.apply(Intent::InsertNewline);
    editor.apply(Intent::MoveWordRight);
    editor.apply(Intent::DeleteWordBackward);
    assert_eq!(text(&editor), "run\n  tests\nthen commit");
    assert_eq!(
      editor.apply(Intent::Submit),
      Outcome::Submit("run\n  tests\nthen commit".to_string())
    );
    // And the recall gives back exactly what was sent, line breaks included.
    editor.apply(Intent::MoveUp);
    assert_eq!(text(&editor), "run\n  tests\nthen commit");
  }
}
