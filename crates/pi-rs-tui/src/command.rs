//! Syntax-aware input classification.
//!
//! Two rules shape this module.
//!
//! **It classifies, it does not interpret.** No command registry, no argument
//! validation, no execution decision. The editor needs to know which characters to
//! highlight while the user is still typing, and a parser that validated commands
//! would have to know the command set, which belongs to whoever dispatches them.
//!
//! **It never touches the filesystem.** Path-ness is decided from shape alone.
//! This runs per keystroke on the render path, and a `stat` per token would make
//! typing latency depend on disk and on whether the user has a slow mount in their
//! home directory. The cost is honest and stated: `/notes` is classified as a path
//! whether or not it exists, and `a/b` is classified as a path even when it is a
//! ratio.
//!
//! Output is spans, not tokens, because the caller renders the original string:
//! re-serializing tokens is how quoting gets silently eaten.

use std::ops::Range;

use crate::style::Role;

/// A classified run of the original input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
  pub range: Range<usize>,
  pub role: Role,
  pub text: String,
}

impl Span {
  /// Slice `source` at `range`. Ranges are always absolute into the caller's full
  /// input, so `text` is derived rather than passed: a span whose text was
  /// re-supplied by hand is how a span and its range silently disagree.
  fn new(source: &str, range: Range<usize>, role: Role) -> Self {
    Self {
      text: source[range.clone()].to_string(),
      range,
      role,
    }
  }
}

/// A slash command or a plain prompt, with the spans needed to highlight it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
  /// `/name arg --flag value`
  Command {
    /// Command name without the leading `/`.
    name: String,
    /// Everything after the name, unmodified.
    rest: String,
    spans: Vec<Span>,
  },
  /// Free-form text sent to the model.
  Prompt { spans: Vec<Span> },
}

impl Input {
  /// Classify one line of editor input.
  pub fn parse(input: &str) -> Self {
    if let Some(after_slash) = input.strip_prefix('/') {
      let name_end = after_slash
        .char_indices()
        .find(|(_, c)| c.is_whitespace())
        .map(|(i, _)| i + 1)
        .unwrap_or(input.len());
      let name = &input[1..name_end];
      if is_command_name(name) {
        let rest_start = name_end;
        let rest = &input[rest_start..];
        let mut spans = vec![Span::new(input, 0..name_end, Role::Operation)];
        spans.extend(classify(input, rest_start, false));
        return Self::Command {
          name: name.to_string(),
          rest: rest.trim_start().to_string(),
          spans,
        };
      }
    }
    Self::Prompt {
      spans: classify(input, 0, true),
    }
  }

  /// `true` when the input is a slash command.
  pub fn is_command(&self) -> bool {
    matches!(self, Self::Command { .. })
  }

  /// Command name without `/`, `None` for a prompt.
  pub fn command_name(&self) -> Option<&str> {
    match self {
      Self::Command { name, .. } => Some(name),
      Self::Prompt { .. } => None,
    }
  }

  /// Highlight spans in ascending, non-overlapping order.
  pub fn spans(&self) -> &[Span] {
    match self {
      Self::Command { spans, .. } | Self::Prompt { spans } => spans,
    }
  }

  /// The input rendered as segments, ready for a [`crate::line::RenderLine`].
  pub fn segments(&self) -> Vec<(String, Role)> {
    self
      .spans()
      .iter()
      .map(|span| (span.text.clone(), span.role))
      .collect()
  }
}

/// Tokens that read as commands: short, lowercase-ish, no path separators.
fn is_command_name(name: &str) -> bool {
  !name.is_empty()
    && !name.contains('/')
    && name
      .chars()
      .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Split `text` into whitespace-separated runs and classify each.
///
/// `prompt_mode` keeps whitespace out of the spans for a command (`/name` and its
/// arguments are separate spans) while a plain prompt is covered completely, so a
/// caller painting the whole line never has to guess about gaps.
fn classify(input: &str, offset: usize, prompt_mode: bool) -> Vec<Span> {
  let text = &input[offset..];
  let mut spans = Vec::new();
  let mut cursor = 0usize;
  while cursor < text.len() {
    let rest = &text[cursor..];
    let lead = rest.len() - rest.trim_start().len();
    if lead > 0 {
      if prompt_mode {
        spans.push(Span::new(
          input,
          offset + cursor..offset + cursor + lead,
          Role::UserText,
        ));
      }
      cursor += lead;
    }
    let token_end = token_end(text, cursor);
    if token_end > cursor {
      let token = &text[cursor..token_end];
      push_token(&mut spans, input, offset + cursor, token);
      cursor = token_end;
    }
  }
  spans
}

fn push_token(spans: &mut Vec<Span>, input: &str, at: usize, token: &str) {
  let end = at + token.len();
  // A quoted token is one claim about one thing; the quotes are part of the text
  // the user typed and stay in the span.
  if is_quoted(token) {
    spans.push(Span::new(input, at..end, classify_token(token)));
    return;
  }
  // `--flag=value` is two claims: a flag and something fed to it. Painting them
  // the same colour is how `--path=/tmp/x` stops being readable at a glance. The
  // `=` travels with the value so the two spans still cover the token exactly.
  if let Some(eq) = inline_value_separator(token) {
    spans.push(Span::new(input, at..at + eq, Role::Flag));
    spans.push(Span::new(
      input,
      at + eq..end,
      classify_value_role(&token[eq + 1..]),
    ));
    return;
  }
  spans.push(Span::new(input, at..end, classify_token(token)));
}

/// End of the token starting at `from`.
///
/// Whitespace separates tokens *except inside a quote*. Splitting on every space is
/// what makes `"my dir/file.rs"` read as two tokens, and a half-token classified on
/// its own produces a plausible wrong role: `"my` is not a path, `dir/file.rs"` is.
/// An unterminated quote simply runs to the end of the input, which is the honest
/// reading of half-typed input.
///
/// Shared with [`crate::highlight`] so the command classifier and the highlight
/// classifier can never disagree about where a run ends.
pub(crate) fn token_end(text: &str, from: usize) -> usize {
  let mut quote: Option<char> = None;
  for (index, c) in text[from..].char_indices() {
    match quote {
      Some(open) => {
        if c == open {
          quote = None;
        }
      }
      None => {
        if c.is_whitespace() {
          return from + index;
        }
        if c == '"' || c == '\'' {
          quote = Some(c);
        }
      }
    }
  }
  text.len()
}

/// Byte index of the `=` that separates a flag from an inline value.
fn inline_value_separator(token: &str) -> Option<usize> {
  if !token.starts_with('-') || token == "--" || token == "-" {
    return None;
  }
  token.find('=')
}

fn classify_value_role(value: &str) -> Role {
  if is_path_shape(value) {
    Role::Path
  } else {
    Role::Argument
  }
}

/// Classify one whitespace-free token.
fn classify_token(token: &str) -> Role {
  let stripped = strip_quotes(token);
  if stripped.starts_with('-') {
    return Role::Flag;
  }
  classify_value_role(stripped)
}

/// Shape-only path test. See the module docs for why this does not stat anything.
pub(crate) fn is_path_shape(token: &str) -> bool {
  if token.is_empty() {
    return false;
  }
  let trimmed = strip_quotes(token.trim_matches(|c| c == '"' || c == '\''));
  if matches!(trimmed, "." | ".." | "~") {
    return true;
  }
  if trimmed.starts_with('/')
    || trimmed.starts_with("./")
    || trimmed.starts_with("../")
    || trimmed.starts_with("~/")
    || trimmed.starts_with('\\')
  {
    return true;
  }
  // A separator anywhere, or a trailing separator, is enough: `src/`, `a/b`,
  // `C:\repo`.
  trimmed.contains('/') || trimmed.contains('\\')
}

fn strip_quotes(token: &str) -> &str {
  if is_quoted(token) {
    return &token[1..token.len() - 1];
  }
  token
}

fn is_quoted(token: &str) -> bool {
  let bytes = token.as_bytes();
  bytes.len() >= 2
    && ((bytes[0] == b'"' && *bytes.last().unwrap() == b'"')
      || (bytes[0] == b'\'' && *bytes.last().unwrap() == b'\''))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn leading_slash_is_a_command_and_the_name_is_an_operation() {
    let input = Input::parse("/trace --tools");
    assert!(input.is_command());
    assert_eq!(input.command_name(), Some("trace"));
    let spans = input.spans();
    assert_eq!(spans[0].role, Role::Operation);
    assert_eq!(spans[0].text, "/trace");
    assert_eq!(spans[1].role, Role::Flag);
    assert_eq!(spans[1].text, "--tools");
  }

  #[test]
  fn bare_slash_is_a_prompt_not_an_empty_command() {
    let input = Input::parse("/");
    assert!(!input.is_command());
    assert_eq!(input.command_name(), None);
  }

  #[test]
  fn path_like_slashes_are_prompts_not_commands() {
    let input = Input::parse("/home/saehwan/repos/pi-rs");
    assert!(!input.is_command());
    assert!(
      input
        .spans()
        .iter()
        .any(|s| s.role == Role::Path && s.text == "/home/saehwan/repos/pi-rs"),
      "{:?}",
      input.spans()
    );
  }

  #[test]
  fn command_arguments_keep_paths_and_flags_apart() {
    let input = Input::parse("/run --config cfg.json crates/pi-rs-core/src/lib.rs");
    let roles: Vec<Role> = input.spans().iter().map(|s| s.role).collect();
    assert_eq!(
      roles,
      vec![Role::Operation, Role::Flag, Role::Argument, Role::Path],
      "{:?}",
      input.spans()
    );
  }

  #[test]
  fn inline_flag_values_split_into_flag_and_value() {
    let input = Input::parse("/run --cwd=/tmp/ws");
    let spans = input.spans();
    assert_eq!(spans[1].role, Role::Flag);
    assert_eq!(spans[1].text, "--cwd");
    assert_eq!(spans[2].role, Role::Path);
    assert_eq!(spans[2].text, "=/tmp/ws");
  }

  #[test]
  fn spans_cover_the_original_text_in_order() {
    let text = "/run  --config   cfg.json";
    let input = Input::parse(text);
    let mut covered = String::new();
    let mut previous_end = 0usize;
    for span in input.spans() {
      assert!(span.range.start >= previous_end, "{:?}", input.spans());
      assert_eq!(
        &text[span.range.clone()],
        span.text,
        "span must be a slice of the input"
      );
      previous_end = span.range.end;
      covered.push_str(&span.text);
    }
    // Command spans deliberately skip separator whitespace, but every
    // non-whitespace byte must be covered exactly once, in order.
    assert_eq!(
      covered,
      text
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
    );
  }

  #[test]
  fn prompts_are_fully_covered_and_highlight_paths() {
    let text = "fix the bug in src/main.rs please";
    let input = Input::parse(text);
    let covered: usize = input.spans().iter().map(|s| s.range.len()).sum();
    assert_eq!(covered, text.len());
    assert!(
      input
        .spans()
        .iter()
        .any(|s| s.role == Role::Path && s.text == "src/main.rs"),
      "{:?}",
      input.spans()
    );
  }

  #[test]
  fn quoted_paths_keep_their_shape() {
    let input = Input::parse("/read --path \"my dir/file.rs\"");
    assert!(
      input
        .spans()
        .iter()
        .any(|s| s.role == Role::Path && s.text.contains("my dir/file.rs")),
      "{:?}",
      input.spans()
    );
  }

  #[test]
  fn windows_paths_are_paths() {
    assert!(is_path_shape("C:\\repo\\src"));
    assert!(is_path_shape("src/"));
    assert!(!is_path_shape("cfg.json"));
    assert!(!is_path_shape("workspace"));
  }

  #[test]
  fn parsing_is_idempotent_and_never_panics_on_odd_input() {
    for text in [
      "/",
      "//",
      "/-",
      "/--",
      "/cmd ",
      "/cmd  ",
      "  /cmd",
      "-",
      "--",
      "--=x",
      "\"",
      "'",
      "/cmd --flag=",
      "/cmd a=b=c",
      "\u{1f600}/x",
    ] {
      let parsed = Input::parse(text);
      assert_eq!(Input::parse(text), parsed);
    }
  }
}
