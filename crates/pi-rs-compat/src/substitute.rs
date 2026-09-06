//! Substituting arguments into a prompt template, using Pi's grammar and only Pi's.
//!
//! The whole grammar, from Pi's documentation:
//!
//! ```text
//! $1, $2, ...          positional arguments
//! $@ or $ARGUMENTS     all arguments joined
//! ${1:-default}        argument 1 when present and non-empty, otherwise the default
//! ${@:-default}        all arguments when present, otherwise the default
//! ${ARGUMENTS:-d}      same, spelled out
//! ${@:N}               arguments from the Nth position (1-indexed)
//! ${@:N:L}             L arguments starting at N
//! ```
//!
//! Two rules this module adds because the documentation does not decide them, and both
//! are the choice that fails loudest rather than the one that fails quietly:
//!
//! - A placeholder that parses as none of the above stays in the output **exactly as it
//!   was written**. A template is prose, and prose can contain a shell `$HOME` or a bare
//!   `$` that was never meant as a placeholder. Silently deleting it would rewrite the
//!   template; inventing an error would reject text Pi accepts.
//! - A positional argument that was not supplied becomes the empty string, as it does in
//!   a shell. `${1:-default}` exists precisely so an author can say what missing means.
//!   Since `$1` is a placeholder, so is every digit after a `$`: a template that means a
//!   literal price has nowhere documented to say so.

/// Substitute `args` into `body`.
pub fn substitute(body: &str, args: &[&str]) -> String {
  let mut out = String::with_capacity(body.len() + 16);
  let mut i = 0;
  while i < body.len() {
    if body.as_bytes()[i] != b'$' {
      let next = next_char_boundary(body, i);
      out.push_str(&body[i..next]);
      i = next;
      continue;
    }
    match expand_at(body, i, args) {
      Some((replacement, end)) => {
        out.push_str(&replacement);
        i = end;
      }
      None => {
        let next = next_char_boundary(body, i);
        out.push_str(&body[i..next]);
        i = next;
      }
    }
  }
  out
}

/// One `$` start of a placeholder: the text it becomes, and where it ends. `None` when
/// what follows the `$` is not a placeholder this grammar knows.
fn expand_at(body: &str, at: usize, args: &[&str]) -> Option<(String, usize)> {
  let bytes = body.as_bytes();
  let after = at + 1;
  if after >= body.len() {
    return None;
  }
  if bytes[after] == b'{' {
    return body[after + 1..].find('}').map(|offset| {
      (
        braced(&body[after + 1..after + 1 + offset], args),
        after + 2 + offset,
      )
    });
  }
  if bytes[after] == b'@' {
    return Some((join(args), after + 1));
  }
  if bytes[after].is_ascii_digit() {
    let end = digits_end(bytes, after);
    let index = body[after..end].parse::<usize>().ok()?;
    return Some((positional(args, index), end));
  }
  // `$ARGUMENTS` is the only word this grammar reads, because it is the only word Pi
  // documents. `$ARGUMENTSUFFIX` is not a word it knows, and stays as written.
  if body[after..].starts_with("ARGUMENTS") && !is_word_byte(bytes.get(after + 9).copied()) {
    return Some((join(args), after + 9));
  }
  None
}

/// The inside of `${...}`.
fn braced(inner: &str, args: &[&str]) -> String {
  let (target, spec) = match inner.split_once(':') {
    Some((target, spec)) => (target, Some(spec)),
    None => (inner, None),
  };
  let all = target == "@" || target == "ARGUMENTS";
  if !all && !is_digits(target) {
    return verbatim(inner);
  }
  let index = if all {
    0
  } else {
    target.parse::<usize>().unwrap_or(0)
  };
  let value = || {
    if all {
      join(args)
    } else {
      positional(args, index)
    }
  };
  let Some(spec) = spec else { return value() };
  // The default of `${1:-default}` keeps its leading `-`: the split consumed the colon.
  if let Some(default) = spec.strip_prefix('-') {
    let value = value();
    return if value.is_empty() {
      default.to_string()
    } else {
      value
    };
  }
  if all {
    return slice_from(args, spec).unwrap_or_else(|| verbatim(inner));
  }
  verbatim(inner)
}

fn verbatim(inner: &str) -> String {
  format!("${{{inner}}}")
}

/// Position `index`, 1-indexed. A position nothing was supplied for is empty.
fn positional(args: &[&str], index: usize) -> String {
  args
    .get(index.saturating_sub(1))
    .copied()
    .unwrap_or("")
    .to_string()
}

/// `N`, or `N:L`, applied to the argument list. Running off the end is nothing, not a
/// failure: `${@:3}` with two arguments means "nothing from here on", as in a shell.
fn slice_from(args: &[&str], spec: &str) -> Option<String> {
  let (start, length) = spec
    .split_once(':')
    .map_or((spec, None), |s| (s.0, Some(s.1)));
  let start: usize = start.parse().ok().filter(|n| *n > 0)?;
  let from = &args[(start - 1).min(args.len())..];
  match length {
    Some(length) => {
      let length: usize = length.parse().ok()?;
      Some(join(&from[..length.min(from.len())]))
    }
    None => Some(join(from)),
  }
}

fn join(args: &[&str]) -> String {
  args.join(" ")
}

fn is_digits(text: &str) -> bool {
  !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_word_byte(byte: Option<u8>) -> bool {
  byte.is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn digits_end(bytes: &[u8], from: usize) -> usize {
  let mut i = from;
  while i < bytes.len() && bytes[i].is_ascii_digit() {
    i += 1;
  }
  i
}

/// The next character boundary after `from`. Byte-wise scanning would otherwise step
/// into the middle of a multibyte character and panic on the slice.
fn next_char_boundary(text: &str, from: usize) -> usize {
  let mut i = from + 1;
  while i < text.len() && !text.is_char_boundary(i) {
    i += 1;
  }
  i
}

#[cfg(test)]
mod tests {
  use super::*;

  fn args(text: &str) -> Vec<&str> {
    if text.is_empty() {
      Vec::new()
    } else {
      text.split(' ').collect()
    }
  }

  fn expand(body: &str, arguments: &str) -> String {
    substitute(body, &args(arguments))
  }

  #[test]
  fn positional_arguments_are_substituted_by_number() {
    assert_eq!(expand("$1 then $2", "first second"), "first then second");
    // Multi-digit positions are numbers, not a digit followed by literal text.
    let ten = (1..=10)
      .map(|n| n.to_string())
      .collect::<Vec<_>>()
      .join(" ");
    assert_eq!(substitute("$10", &args(&ten)), "10");
    // A position with nothing in it becomes empty, as in a shell.
    assert_eq!(expand("[$3]", "only two"), "[]");
    assert_eq!(expand("$1$2", "a b"), "ab");
  }

  #[test]
  fn the_whole_argument_list_has_two_spellings() {
    assert_eq!(expand("features: $@", "a b c"), "features: a b c");
    assert_eq!(expand("features: $ARGUMENTS", "a b c"), "features: a b c");
    assert_eq!(expand("[$@]", ""), "[]");
  }

  #[test]
  fn a_default_is_used_when_the_value_is_missing_or_empty() {
    assert_eq!(expand("${1:-seven}", ""), "seven");
    assert_eq!(expand("${1:-seven}", "three"), "three");
    // Present *and* non-empty is the rule, so an empty argument takes the default too.
    assert_eq!(expand("${1:-seven}", " "), "seven");
    assert_eq!(expand("${@:-none}", ""), "none");
    assert_eq!(expand("${@:-none}", "a b"), "a b");
    assert_eq!(expand("${ARGUMENTS:-none}", ""), "none");
    // An empty default is a default: it says "nothing here" on purpose.
    assert_eq!(expand("[${1:-}]", ""), "[]");
  }

  #[test]
  fn the_argument_list_can_be_sliced() {
    assert_eq!(expand("${@:2}", "a b c d"), "b c d");
    assert_eq!(expand("${@:2:2}", "a b c d"), "b c");
    assert_eq!(expand("${@:1:1}", "a b c"), "a");
    // Positions are 1-indexed, and running off the end is nothing, not a failure.
    assert_eq!(expand("[${@:5}]", "a b"), "[]");
    assert_eq!(expand("[${@:1:99}]", "a b"), "[a b]");
  }

  #[test]
  fn text_that_is_not_a_placeholder_survives_exactly() {
    for body in [
      "echo $HOME",
      "$",
      "trailing $ ",
      "${unclosed",
      "${not a placeholder}",
      "${1:2}",
      "${@:0}",
      "$ARGUMENTSX",
      "a $ b",
    ] {
      assert_eq!(expand(body, "x y"), body, "{body}");
    }
    // `$10.00` above is a price only because there is no tenth argument: the same text
    // with ten arguments is a placeholder, which is why leaving it alone is the only
    // honest choice rather than a guess.
    let ten = (1..=10)
      .map(|i| i.to_string())
      .collect::<Vec<_>>()
      .join(" ");
    assert_eq!(
      substitute("costs $10.00 flat", &args(&ten)),
      "costs 10.00 flat"
    );
  }

  #[test]
  fn substitution_does_not_cut_a_multibyte_character_in_half() {
    // The scanner advances byte-wise, so a body whose placeholders sit against
    // non-ASCII text is the case that would panic if it stepped into a character.
    assert_eq!(expand("héllo → $1 ✓ $@", "un deux"), "héllo → un ✓ un deux");
    assert_eq!(expand("日本語 $1", "one"), "日本語 one");
    assert_eq!(expand("é${1:-x}", ""), "éx");
  }

  #[test]
  fn a_dollar_amount_in_a_template_is_read_as_a_placeholder() {
    // `$1` is a placeholder, so any digit after `$` is one: with two arguments supplied,
    // "costs $10.00" is position ten followed by ".00", and the price disappears. That is
    // the documented grammar, not a quirk of this reader -- which is why the grammar says
    // what it does about digits, and why nothing here quietly spares the author the
    // consequence. An author who means a literal dollar sign writes one the grammar does
    // not read (`$HOME`, `$ x`); there is no documented backslash escape to invent.
    assert_eq!(expand("costs $10.00 flat", "x y"), "costs .00 flat");
    assert_eq!(expand("costs $5.00 flat", "x y"), "costs .00 flat");
  }

  #[test]
  fn a_body_with_no_placeholders_is_returned_as_it_was_stored() {
    let body = "Review the staged changes.\n- bugs\n- security";
    assert_eq!(expand(body, "ignored"), body);
  }
}
