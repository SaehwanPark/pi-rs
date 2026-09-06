//! Display-width measurement and width-aware line breaking.
//!
//! Column arithmetic is a correctness problem, not a polish problem: a renderer
//! that believes a CJK glyph is one column wide writes one cell past the edge of
//! the terminal, and the result is a transcript that drifts rightwards on
//! perfectly ordinary Korean or Japanese text.
//!
//! `unicode-width` would give exact UAX #11 data. It is deliberately not a
//! dependency: the runtime does not need exactness for every codepoint, it needs
//! *never undercounting* in the ranges a coding agent actually prints (ASCII,
//! Latin-1 accented prose, CJK, Hangul, box drawing, emoji) and never panicking
//! elsewhere. Undercounting is what corrupts layout; overcounting by one cell in
//! a rare symbol only wastes a cell.
//!
//! Rules implemented here:
//!
//! - control characters and other non-printables measure `0`;
//! - combining marks and Hangul medial/final jamo measure `0` because they
//!   overlap the preceding grapheme;
//! - East Asian Wide and Fullwidth ranges measure `2`;
//! - emoji-presentation ranges measure `2`, matching what mainstream terminals do;
//! - everything else measures `1`.
//!
//! Measurement is per `char`, so a grapheme cluster of base plus combining marks
//! measures the base character and contributes `0` for the marks, which is the
//! same answer a correct cluster measurement would give for these ranges.

/// Columns a single scalar value occupies.
pub fn char_width(c: char) -> usize {
  let cp = c as u32;
  if is_control(cp) || is_zero_width(cp) {
    0
  } else if is_wide(cp) {
    2
  } else {
    1
  }
}

/// Columns `text` occupies.
pub fn display_width(text: &str) -> usize {
  text.chars().map(char_width).sum()
}

/// `true` when `text` fits in `width` columns.
pub fn fits(text: &str, width: usize) -> bool {
  display_width(text) <= width
}

/// Truncate to `width` columns, appending `…` when anything was removed.
///
/// The ellipsis is budgeted for, so the result never exceeds `width`, including
/// when `width` is `0` (empty output) or `1` (just the ellipsis).
pub fn truncate(text: &str, width: usize) -> String {
  if display_width(text) <= width {
    return text.to_string();
  }
  if width == 0 {
    return String::new();
  }
  let budget = width - char_width('…');
  let mut out = String::new();
  let mut used = 0usize;
  for c in text.chars() {
    let w = char_width(c);
    if used + w > budget {
      break;
    }
    used += w;
    out.push(c);
  }
  out.push('…');
  out
}

/// Break `text` into lines no wider than `width` columns.
///
/// Existing hard newlines are preserved. Words break on whitespace when possible
/// and hard-split only when a single word cannot fit, which is what keeps long
/// paths and base64 blobs readable instead of silently dropped.
///
/// Below two columns there is no room for a word plus a separator, so `width`
/// that small switches to the narrow fallback: hard splits only. `MIN_COLUMN` is
/// *not* that threshold — it gates decoration, not word alignment, and applying it
/// here would hard-split ordinary prose in a 10-column pipe.
///
/// The one unavoidable overflow is a single double-width glyph at `width < 2`: it
/// occupies two columns wherever it goes, so callers budget `width.max(2)`.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
  let width = width.max(1);
  let narrow = width < 2;
  let mut lines = Vec::new();
  for raw in text.split('\n') {
    if raw.is_empty() {
      lines.push(String::new());
      continue;
    }
    if narrow {
      lines.extend(hard_wrap(raw, width));
      continue;
    }
    lines.extend(word_wrap(raw, width));
  }
  lines
}

/// Narrowest column count that still deserves full decoration.
///
/// Below this, prefix-plus-content arithmetic stops being meaningful: an indent
/// of two columns is a fifth of a twelve-column line. Callers use it to drop
/// decoration rather than to refuse to render.
pub const MIN_COLUMN: usize = 12;

fn word_wrap(text: &str, width: usize) -> Vec<String> {
  let mut lines = Vec::new();
  let mut line = String::new();
  let mut line_width = 0usize;
  for word in text.split_whitespace() {
    let word_width = display_width(word);
    if word_width > width {
      if !line.is_empty() {
        lines.push(std::mem::take(&mut line));
        line_width = 0;
      }
      lines.extend(hard_wrap(word, width));
      continue;
    }
    let sep_width = if line.is_empty() { 0 } else { 1 };
    if line_width + sep_width + word_width > width {
      lines.push(std::mem::take(&mut line));
      line.push_str(word);
      line_width = word_width;
    } else {
      if sep_width == 1 {
        line.push(' ');
      }
      line.push_str(word);
      line_width += sep_width + word_width;
    }
  }
  if !line.is_empty() {
    lines.push(line);
  }
  if lines.is_empty() {
    lines.push(String::new());
  }
  lines
}

fn hard_wrap(text: &str, width: usize) -> Vec<String> {
  let width = width.max(1);
  let mut lines = Vec::new();
  let mut line = String::new();
  let mut line_width = 0usize;
  for c in text.chars() {
    let w = char_width(c);
    // A double-width glyph does not fit in a one-column remainder; move it to a
    // fresh line rather than overflowing the column budget by one.
    if line_width + w > width && line_width > 0 {
      trim(&mut line);
      lines.push(std::mem::take(&mut line));
      line_width = 0;
    }
    // Whitespace at a hard-break boundary renders as nothing and, kept, would make
    // the line wider than it reads. Only interior spaces survive.
    if line.is_empty() && c == ' ' {
      continue;
    }
    line.push(c);
    line_width += w;
  }
  trim(&mut line);
  if !line.is_empty() {
    lines.push(line);
  }
  lines
}

/// Drop trailing spaces from a hard-wrapped line without touching its start: an
/// indent is content, a dangling space is not.
fn trim(line: &mut String) {
  let kept = line.trim_end().len();
  if kept < line.len() {
    line.truncate(kept);
  }
}

fn is_control(cp: u32) -> bool {
  cp < 0x20 || (0x7f..=0x9f).contains(&cp)
}

fn is_zero_width(cp: u32) -> bool {
  matches!(
    cp,
    // Combining diacritical marks and their supplements.
    0x0300..=0x036f |
    0x1ab0..=0x1aff |
    0x1dc0..=0x1dff |
    0x20d0..=0x20f0 |
    0xfe00..=0xfe0f |
    0xfe20..=0xfe2f |
    // Hangul medial and final jamo compose with a leading jamo.
    0x1160..=0x11ff |
    // Word joiners and the zero-width space family.
    0x200b..=0x200f |
    0x2060..=0x2064 |
    0xfeff
  )
}

fn is_wide(cp: u32) -> bool {
  matches!(
    cp,
    // Hangul jamo leading consonants.
    0x1100..=0x115f |
    // CJK punctuation, kana, ideographs, and the compat blocks.
    0x2e80..=0x303e |
    0x3041..=0x33ff |
    0x3400..=0x4dbf |
    0x4e00..=0x9fff |
    0xa000..=0xa4cf |
    0xa960..=0xa97f |
    0xac00..=0xd7a3 |
    0xf900..=0xfaff |
    0xfe10..=0xfe19 |
    0xfe30..=0xfe6f |
    // Fullwidth forms.
    0xff00..=0xff60 |
    0xffe0..=0xffe6 |
    // Emoji that terminals render double-width by default.
    0x1f300..=0x1f64f |
    0x1f680..=0x1f6ff |
    0x1f7e0..=0x1f7eb |
    0x1f900..=0x1f9ff |
    0x20000..=0x3fffd
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn ascii_measures_one_column_per_character() {
    assert_eq!(display_width("read file"), 9);
    assert_eq!(display_width(""), 0);
  }

  #[test]
  fn cjk_and_hangul_measure_two_columns() {
    assert_eq!(display_width("中文"), 4);
    assert_eq!(display_width("한글"), 4);
    assert_eq!(display_width("日本語テキスト"), 14);
    assert_eq!(display_width("ｆｕｌｌ"), 8);
  }

  #[test]
  fn emoji_measure_two_columns_and_marks_measure_zero() {
    assert_eq!(display_width("🧠"), 2);
    assert_eq!(display_width("é"), 1);
    assert_eq!(display_width("\u{200d}"), 0);
    assert_eq!(display_width("\u{fe0f}"), 0);
  }

  #[test]
  fn control_characters_measure_zero() {
    assert_eq!(display_width("\n\t\x00\u{7f}"), 0);
  }

  #[test]
  fn truncate_budgets_for_the_ellipsis() {
    assert_eq!(truncate("abcdef", 6), "abcdef");
    assert_eq!(truncate("abcdef", 4), "abc…");
    assert_eq!(truncate("abcdef", 1), "…");
    assert_eq!(truncate("abcdef", 0), "");
    assert_eq!(truncate("中文字", 4), "中…");
    assert!(display_width(&truncate("中文字", 4)) <= 4);
  }

  #[test]
  fn wrap_preserves_hard_newlines() {
    assert_eq!(wrap("a\nb", 40), vec!["a".to_string(), "b".to_string()]);
    assert_eq!(wrap("", 40), vec![String::new()]);
  }

  #[test]
  fn wrap_breaks_on_words() {
    assert_eq!(
      wrap("the quick brown fox", 10),
      vec!["the quick", "brown fox"]
    );
  }

  #[test]
  fn wrap_hard_splits_words_that_cannot_fit() {
    let lines = wrap("aaaaaaaaaaaa", 5);
    assert!(lines.iter().all(|l| display_width(l) <= 5), "{lines:?}");
    assert_eq!(lines.concat(), "aaaaaaaaaaaa");
  }

  #[test]
  fn wrap_never_overflows_the_column_budget() {
    let text = "read crates/pi-rs-core/src/event.rs · 中文説明 · 🧠 · a=1";
    for width in 1..40 {
      for line in wrap(text, width) {
        // A double-width glyph cannot be narrowed below two columns; everything
        // else must respect the budget exactly.
        assert!(
          display_width(&line) <= width.max(2),
          "width {width}: {line:?}"
        );
      }
    }
  }

  #[test]
  fn wrap_does_not_split_a_double_width_glyph_across_lines() {
    for line in wrap("中文中", 4) {
      assert!(display_width(&line) <= 4, "{line:?}");
    }
    assert_eq!(wrap("中文中", 4).concat(), "中文中");
  }

  #[test]
  fn narrow_width_uses_the_hard_fallback() {
    // Below two columns there is no word alignment to lose.
    let lines = wrap("one two three", 1);
    assert!(lines.iter().all(|l| display_width(l) <= 1), "{lines:?}");
    assert_eq!(
      lines,
      vec!["o", "n", "e", "t", "w", "o", "t", "h", "r", "e", "e"]
    );
  }

  #[test]
  fn word_alignment_survives_a_narrow_pipe() {
    // Ten columns is a real budget, not a degenerate one: breaking mid-word here
    // would be the renderer losing information it was never asked to lose.
    assert_eq!(
      wrap("the quick brown fox", 10),
      vec!["the quick", "brown fox"]
    );
  }

  #[test]
  fn hard_wrap_does_not_leave_dangling_spaces() {
    let lines = wrap("one two three", 1);
    assert!(
      lines.iter().all(|l| l.is_empty() || !l.ends_with(' ')),
      "{lines:?}"
    );
    assert_eq!(wrap("a b c", 1), vec!["a", "b", "c"]);
  }
}
