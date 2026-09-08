//! The operation/argument split of a single input line.
//!
//! This module answers one question and nothing else: which run of the line is
//! the thing being *done*, and which runs are the things it is done *to*. That
//! is the whole colour decision for a typed line. The editor and the live
//! surface are deliberately untouched here; wiring these segments into them is a
//! separate slice, and this projection has to be proved before it is painted.
//!
//! [`crate::command`] already tokenises a line, but for a different question: it
//! pulls flags and paths out of the arguments and covers a plain prompt
//! completely. This module keeps the coarser split the highlighting rule needs,
//! and reuses [`token_end`] so the two can never drift apart on what a run is.

use crate::{command::token_end, line::Segment, style::Role};

/// Split `input` into the operation it names and the arguments that follow it.
///
/// - the first non-whitespace run is [`Role::Operation`]; every later run is
///   [`Role::Argument`]. No other role is emitted: at this point nothing else is
///   known about a run, and guessing flag or path from shape here would be a
///   second, contradicting opinion next to [`crate::command`];
/// - runs are separated by whitespace, and a quoted run — `'…'` or `"…"` — is
///   **one** run even when it contains spaces. The quote characters stay in the
///   segment text;
/// - an unterminated quote is not an error: the run takes the rest of the input
///   and remains an argument;
/// - backslashes are literal. There is no escape processing, no expansion, and
///   no `$(…)` or `$VAR` interpretation. This is a classifier for colour, never
///   a claim about argv;
/// - empty or whitespace-only input yields an empty `Vec` — no phantom segment,
///   no panic.
///
/// Separators are **not** emitted. The result is the token list, not a copy of
/// the line: joining the segment texts back together does not reproduce the
/// original spacing, and exact input reproduction is not a goal of this
/// function. A caller that must repaint the original line verbatim has to keep
/// the source line and slice it itself.
///
/// Pure: no I/O, no allocation beyond the returned segments, and no display
/// width assumptions — width is the caller's problem, see [`crate::width`].
///
/// [`token_end`]: crate::command::token_end
pub fn tokens(input: &str) -> Vec<Segment> {
  let mut segments = Vec::new();
  let mut cursor = 0usize;
  while cursor < input.len() {
    let rest = &input[cursor..];
    cursor += rest.len() - rest.trim_start().len();
    if cursor == input.len() {
      break;
    }
    // `cursor` sits on a non-whitespace character here, so `token_end` always
    // moves forward and the loop terminates.
    let end = token_end(input, cursor);
    let role = if segments.is_empty() {
      Role::Operation
    } else {
      Role::Argument
    };
    segments.push(Segment::new(&input[cursor..end], role));
    cursor = end;
  }
  segments
}

#[cfg(test)]
mod tests {
  use super::*;

  fn texts(input: &str) -> Vec<String> {
    tokens(input)
      .into_iter()
      .map(|segment| segment.text)
      .collect()
  }

  fn roles(input: &str) -> Vec<Role> {
    tokens(input)
      .into_iter()
      .map(|segment| segment.role)
      .collect()
  }

  #[test]
  fn operation_only_input_is_one_operation_segment() {
    assert_eq!(texts("read"), ["read"]);
    assert_eq!(roles("read"), [Role::Operation]);
  }

  #[test]
  fn the_first_run_is_the_operation_and_the_rest_are_arguments() {
    assert_eq!(
      texts("read crates/pi-rs-tui/src/lib.rs 12"),
      ["read", "crates/pi-rs-tui/src/lib.rs", "12"]
    );
    assert_eq!(
      roles("read crates/pi-rs-tui/src/lib.rs 12"),
      [Role::Operation, Role::Argument, Role::Argument]
    );
  }

  #[test]
  fn a_double_quoted_run_with_a_space_is_one_argument_that_keeps_its_quotes() {
    assert_eq!(
      texts(r#"commit -m "fix the parser""#),
      ["commit", "-m", r#""fix the parser""#]
    );
    assert_eq!(
      roles(r#"commit -m "fix the parser""#),
      [Role::Operation, Role::Argument, Role::Argument]
    );
  }

  #[test]
  fn single_quotes_group_the_same_way() {
    assert_eq!(texts("ask 'what is 1 + 1'"), ["ask", "'what is 1 + 1'"]);
    assert_eq!(
      roles("ask 'what is 1 + 1'"),
      [Role::Operation, Role::Argument]
    );
  }

  #[test]
  fn adjacent_quotes_stay_one_argument() {
    assert_eq!(texts(r#"open "a"b"#), ["open", r#""a"b"#]);
    assert_eq!(texts(r#"open x"a"b y"#), ["open", "x\"a\"b", "y"]);
  }

  #[test]
  fn an_unterminated_quote_runs_to_the_end_of_the_input() {
    assert_eq!(texts(r#"grep "half typed"#), ["grep", r#""half typed"#]);
    assert_eq!(
      roles(r#"grep "half typed"#),
      [Role::Operation, Role::Argument]
    );
    assert_eq!(texts("grep '"), ["grep", "'"]);
  }

  #[test]
  fn leading_whitespace_does_not_move_the_operation() {
    assert_eq!(texts("   \t read lib.rs"), ["read", "lib.rs"]);
    assert_eq!(
      roles("   \t read lib.rs"),
      [Role::Operation, Role::Argument]
    );
  }

  #[test]
  fn empty_and_whitespace_only_inputs_have_no_segments() {
    assert_eq!(tokens(""), Vec::<Segment>::new());
    assert_eq!(tokens("   \t\n "), Vec::<Segment>::new());
  }

  #[test]
  fn a_non_ascii_argument_is_not_split_on_byte_boundary_characters() {
    assert_eq!(texts("read → 日本語 ✨"), ["read", "→", "日本語", "✨"]);
    assert_eq!(texts("grep 'héllo wörld'"), ["grep", "'héllo wörld'"]);
    assert_eq!(
      roles("read → 日本語 ✨"),
      [
        Role::Operation,
        Role::Argument,
        Role::Argument,
        Role::Argument
      ]
    );
  }

  #[test]
  fn backslashes_are_literal_and_nothing_is_expanded() {
    assert_eq!(texts(r"echo C:\path\now"), ["echo", r"C:\path\now"]);
    assert_eq!(texts("echo $(ls) $HOME"), ["echo", "$(ls)", "$HOME"]);
  }

  /// The reconstruction rule this slice took: separators are not emitted, so the
  /// check is that the segment texts, in order, equal the expected token list.
  /// Exact input reproduction is deliberately *not* asserted, because this
  /// function cannot and does not promise it.
  #[test]
  fn segment_texts_are_the_expected_token_list() {
    let table: [(&str, &[&str]); 8] = [
      ("read", &["read"]),
      (
        "read crates/pi-rs-tui/src/lib.rs",
        &["read", "crates/pi-rs-tui/src/lib.rs"],
      ),
      (
        r#"commit -m "one two" 'three four'"#,
        &["commit", "-m", r#""one two""#, "'three four'"],
      ),
      ("   \t spaced  out \n", &["spaced", "out"]),
      ("open \"a\"b tail", &["open", "\"a\"b", "tail"]),
      (r#"echo C:\path\now"#, &["echo", r"C:\path\now"]),
      ("grep 'unterminated", &["grep", "'unterminated"]),
      ("日本 語 → ✨", &["日本", "語", "→", "✨"]),
    ];
    for (input, expected) in table {
      assert_eq!(texts(input).as_slice(), expected, "tokens of {input:?}");
    }
  }
}
