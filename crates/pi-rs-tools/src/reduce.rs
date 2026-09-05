//! Output reduction at the tool boundary.
//!
//! Tool output is the main way a coding agent floods its own context. The rule
//! here is intentionally mechanical and does not depend on model judgment:
//! anything at or above the configured budget becomes a bounded, labelled
//! representation, and the full bytes are handed to the caller so the runtime
//! can archive them and keep a recovery reference.
//!
//! Two properties matter more than cleverness:
//!
//! - **Recoverable.** The reduction always states how much was elided and that a
//!   full copy exists, so the model can ask for the rest instead of concluding
//!   the output ended.
//! - **Honest.** The reduced form is smaller, says so, and never claims to be
//!   the whole result.

/// A reduced representation plus the bytes it was reduced from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reduction {
  /// Model-visible text.
  pub text: String,
  /// The complete output, for durable archiving.
  pub full: String,
  /// Byte length of [`Self::full`], kept so size claims stay checkable.
  pub full_bytes: u64,
  /// Bytes elided, so the count in the text is also available as data.
  pub elided_bytes: u64,
  /// The byte budget the reduction was built to fit.
  pub budget: u64,
}

/// Reduce `text` when it is at or above `max_bytes`.
///
/// Returns `None` when no reduction was needed, which keeps callers from having
/// to compare lengths themselves.
pub fn reduce(text: &str, max_bytes: u64) -> Option<Reduction> {
  let full_bytes = text.len() as u64;
  if full_bytes < max_bytes {
    return None;
  }
  let budget = (max_bytes as usize).clamp(MIN_USEFUL, MAX_USEFUL);
  // Keep a little more of the head than the tail: the beginning usually carries
  // the command header and the first error, while the tail is often a summary.
  let head_len = budget * 3 / 5;
  let tail_len = budget - head_len;
  let head = floor_char_boundary(text, head_len);
  let tail_start = ceil_char_boundary(text, text.len() - tail_len);
  let elided = tail_start - head;

  let mut reduced = String::with_capacity(budget + 128);
  reduced.push_str(&text[..head]);
  reduced.push_str(&notice(elided, full_bytes));
  reduced.push_str(&text[tail_start..]);

  Some(Reduction {
    text: reduced,
    full: text.to_string(),
    full_bytes,
    elided_bytes: elided as u64,
    budget: max_bytes,
  })
}

/// Below this, a head plus notice plus tail cannot be honest about elision; it
/// is rounded up rather than producing something that looks complete.
const MIN_USEFUL: usize = 1024;
/// Ceiling so a misconfigured huge budget cannot make context worse than the
/// raw output it was meant to bound.
const MAX_USEFUL: usize = 64 * 1024;

/// The elision notice, in one place so that a test can recompute it exactly
/// rather than back-solving it from the reducer's own arithmetic.
fn notice(elided: usize, full_bytes: u64) -> String {
  format!(
    "\n\n[... {elided} bytes elided from {full_bytes} bytes of tool output; \
     the full output is archived, read it by path or re-run with a filter ...]\n\n"
  )
}

fn floor_char_boundary(text: &str, index: usize) -> usize {
  if index >= text.len() {
    return text.len();
  }
  let mut cursor = index;
  while cursor > 0 && !text.is_char_boundary(cursor) {
    cursor -= 1;
  }
  cursor
}

fn ceil_char_boundary(text: &str, index: usize) -> usize {
  if index >= text.len() {
    return text.len();
  }
  let mut cursor = index;
  while cursor < text.len() && !text.is_char_boundary(cursor) {
    cursor += 1;
  }
  cursor
}

#[cfg(test)]
mod tests {
  use super::*;

  fn text(len: usize) -> String {
    // Numbered lines so head/tail retention is visible in the assertions.
    (0..len / 20 + 1)
      .map(|line| format!("line {line:05}: {}\n", "x".repeat(14)))
      .collect()
  }

  #[test]
  fn small_output_is_left_alone() {
    assert!(reduce("short output\n", 8 * 1024).is_none());
  }

  #[test]
  fn large_output_becomes_a_bounded_labeled_form() {
    let source = text(40 * 1024);
    let reduced = reduce(&source, 8 * 1024).expect("reduced");
    assert!(reduced.text.len() < source.len() / 2);
    assert!(reduced.text.contains("bytes elided"));
    assert!(reduced.text.contains(&source[..60]), "head retained");
    assert!(
      reduced.text.contains(&source[source.len() - 60..]),
      "tail retained"
    );
    assert_eq!(reduced.full, source);
    assert_eq!(reduced.full_bytes, source.len() as u64);
  }

  #[test]
  fn the_stated_elision_count_matches_the_bytes_that_disappeared() {
    let source = text(40 * 1024);
    let reduced = reduce(&source, 2 * 1024).expect("reduced");
    let stated: u64 = reduced
      .text
      .split("... ")
      .nth(1)
      .expect("notice")
      .split(" bytes elided")
      .next()
      .expect("count")
      .parse()
      .expect("numeric count");
    assert_eq!(stated, reduced.elided_bytes);
    assert!(stated > 0);
  }

  #[test]
  fn multibyte_text_is_never_split() {
    let source = "é漢".repeat(4_000);
    let reduced = reduce(&source, 1_024).expect("reduced");
    assert!(std::str::from_utf8(reduced.text.as_bytes()).is_ok());
    assert!(
      reduced
        .text
        .chars()
        .all(|c| c == 'é' || c == '漢' || c.is_ascii())
    );
  }

  #[test]
  fn a_tiny_budget_reduces_honestly_by_clamping_up() {
    let source = text(40 * 1024);
    let reduced = reduce(&source, 64).expect("reduced");
    assert!(reduced.text.contains("bytes elided"));
    assert!(reduced.text.len() < source.len());
    assert_eq!(reduced.budget, 64);
    assert!(reduced.text.len() >= MIN_USEFUL);
  }

  #[test]
  fn elision_and_visible_bytes_account_for_the_source() {
    let source = text(40 * 1024);
    let reduced = reduce(&source, 8 * 1024).expect("reduced");
    // visible = head + notice + tail, so hidden + visible exceeds the source by
    // exactly the length of the notice. The accounting has to close, or a size
    // claim in the trace is fabricated: recompute the notice independently from
    // the constants the reducer uses instead of back-solving it.
    let expected = notice(reduced.elided_bytes as usize, source.len() as u64).len() as u64;
    let added = reduced.text.len() as u64 + reduced.elided_bytes - source.len() as u64;
    assert_eq!(added, expected, "the notice must be the only addition");
    assert_eq!(
      reduced.elided_bytes + reduced.text.len() as u64,
      source.len() as u64 + expected,
      "hidden + visible must equal the source plus the notice that replaced it"
    );
    assert_eq!(reduced.full_bytes, source.len() as u64);
  }

  #[test]
  fn the_notice_says_where_the_rest_went() {
    let reduced = reduce(&text(40 * 1024), 1_024).expect("reduced");
    assert!(
      reduced.text.contains("full output is archived"),
      "{}",
      reduced.text
    );
  }
}
