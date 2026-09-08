//! The `---` frontmatter block that opens a Pi skill or prompt file.
//!
//! Pi reads frontmatter with a YAML parser and is deliberately lenient about what it
//! finds. Only a small part of YAML can actually appear in the fields Pi defines: a
//! flat set of `key: value` pairs whose values are short strings, one long string
//! that authors often write as a block scalar, and a boolean. This module reads that
//! part directly, and nothing else.
//!
//! The reasons for not depending on a YAML crate are concrete rather than
//! squeamish: skill discovery runs before the user can type, a full YAML object model
//! is surface area nothing here consumes, and every YAML dependency considered is
//! either unmaintained or larger than the format being read.
//!
//! # What is read
//!
//! - `key: value` at column zero, keys trimmed, first `:` splitting key from value.
//! - Double- and single-quoted values, with the escapes `\\`, `\"` and the SQL-style
//!   `''` unescaped.
//! - Block scalars introduced by `|` or `>` with optional `-` chomping: `|` keeps
//!   line breaks, `>` folds them into spaces. Blank lines inside a block survive as
//!   paragraph breaks.
//! - `---` closing the block. Lines that are blank, comments, indented continuations,
//!   or otherwise not a `key: value` pair are skipped.
//!
//! # What is not
//!
//! - Inline comments after a value. `description: Use # for tags` reads as a
//!   description that says `Use # for tags`, because guessing where a comment starts
//!   in an unquoted string is how a description loses its second half.
//! - Nested mappings, sequences, anchors, and multiple documents. Pi ignores unknown
//!   fields, so a skill carrying them still loads; its unknown fields simply carry no
//!   value here.
//! - A closing `...`, which YAML also accepts. Pi is documented with `---`, and a
//!   file ending that way is reported as unterminated rather than silently truncated.

use std::collections::BTreeMap;

/// Why a frontmatter block could not be read at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Malformed {
  /// A `---` opened the block and nothing closed it, so everything after it is
  /// frontmatter as far as this reader can tell.
  Unterminated,
}

/// The key-value pairs of a frontmatter block.
///
/// Keys are kept exactly as written. Pi defines lowercase-hyphenated keys and ignores
/// unknown ones, so a reader that looks up `description` finds nothing in a file that
/// wrote `Description` -- and that is the correct outcome, because inventing a match
/// would load a file Pi would not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Frontmatter {
  fields: BTreeMap<String, String>,
}

impl Frontmatter {
  /// Value of `key`, if the block declared one.
  pub fn get(&self, key: &str) -> Option<&str> {
    self.fields.get(key).map(String::as_str)
  }

  /// Whether `key` was declared as the plain scalar `true`.
  ///
  /// YAML has several truthy spellings; Pi's documented one is `true`, and accepting
  /// `yes` or `1` would hide a skill from the model on a spelling the author did not
  /// intend.
  pub fn is_true(&self, key: &str) -> bool {
    self.get(key) == Some("true")
  }

  /// Declared keys, in sorted order. For diagnostics.
  pub fn keys(&self) -> impl Iterator<Item = &str> {
    self.fields.keys().map(String::as_str)
  }
}

/// What a file's opening block turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Extract {
  /// A well-formed block. It may still be empty.
  Found(Frontmatter),
  /// No block at the start: this is ordinary prose, which is what most Markdown files
  /// in a skills directory are.
  Absent,
  /// A block was opened and never closed.
  Malformed(Malformed),
}

/// Everything after the closing `---`, or `text` itself when there is no well-formed
/// block to remove.
///
/// A block that never closed is not removed: without a closing line there is no fact
/// about where the metadata ends, and guessing one would silently swallow part of a
/// document. Callers that care distinguish that case through [`extract`], which reports
/// it, and this function is only ever called on text that passed that check.
pub fn strip(text: &str) -> &str {
  let text = text.strip_prefix('\u{feff}').unwrap_or(text);
  let mut lines = text.split_inclusive('\n');
  let Some(open) = lines.next() else {
    return text;
  };
  if open.trim_end().trim() != "---" {
    return text;
  }
  let mut offset = open.len();
  for line in lines {
    if line.trim_end().trim() == "---" {
      return text[offset + line.len()..].trim_start_matches(['\r', '\n']);
    }
    offset += line.len();
  }
  text
}

/// Read the frontmatter block of `text`.
pub fn extract(text: &str) -> Extract {
  let text = text.strip_prefix('\u{feff}').unwrap_or(text);
  let mut lines = text.lines();
  if lines.next().map(str::trim_end) != Some("---") {
    return Extract::Absent;
  }
  let mut fields = BTreeMap::new();
  let mut pending: Option<(String, BlockScalar)> = None;
  for line in lines {
    let trimmed = line.trim_end();
    if trimmed == "---" {
      close_block(&mut pending, &mut fields);
      return Extract::Found(Frontmatter { fields });
    }
    if let Some((_, block)) = pending.as_mut() {
      // A block scalar owns every following line until the indentation stops, and
      // blank lines in between are paragraphs rather than the end of the value.
      if trimmed.is_empty() || line.starts_with([' ', '\t']) {
        block.lines.push(trimmed.to_string());
        continue;
      }
      close_block(&mut pending, &mut fields);
    }
    if trimmed.is_empty() || trimmed.starts_with('#') {
      continue;
    }
    if line.starts_with([' ', '\t']) {
      // An indented line outside a block scalar: continuation of something this
      // reader does not model, or a stray indent. Pi ignores what it does not
      // understand, so this does too.
      continue;
    }
    let Some((key, raw)) = trimmed.split_once(':') else {
      continue;
    };
    let key = key.trim();
    if key.is_empty() {
      continue;
    }
    let raw = raw.trim();
    if let Some(marker) = raw.strip_prefix('|').or_else(|| raw.strip_prefix('>')) {
      let chomp = marker.trim();
      if chomp.is_empty() || chomp == "-" {
        pending = Some((
          key.to_string(),
          BlockScalar {
            fold: raw.starts_with('>'),
            keep_trailing: chomp.is_empty(),
            lines: Vec::new(),
          },
        ));
        continue;
      }
    }
    fields.insert(key.to_string(), unquote(raw));
  }
  close_block(&mut pending, &mut fields);
  Extract::Malformed(Malformed::Unterminated)
}

/// A value written as a block scalar, still collecting its lines.
struct BlockScalar {
  fold: bool,
  keep_trailing: bool,
  lines: Vec<String>,
}

impl BlockScalar {
  /// The value those lines spell.
  ///
  /// Indentation is measured from the first non-blank line, which is what makes a
  /// folded description come out as one line instead of one line per source line.
  fn value(&self) -> String {
    let indent = self
      .lines
      .iter()
      .find(|line| !line.trim().is_empty())
      .map(|line| line.len() - line.trim_start().len())
      .unwrap_or(0);
    let stripped: Vec<&str> = self
      .lines
      .iter()
      .map(|line| line.get(indent..).unwrap_or(line.trim_start()))
      .collect();
    let mut text = if self.fold {
      // A line break between two non-empty lines folds into a space; each blank line
      // survives as a line break of its own.
      let mut folded = String::new();
      let mut blanks = 0usize;
      let mut saw_text = false;
      for line in &stripped {
        if line.trim().is_empty() {
          blanks += 1;
          continue;
        }
        if saw_text {
          if blanks == 0 {
            folded.push(' ');
          } else {
            folded.push_str(&"\n".repeat(blanks));
          }
        }
        folded.push_str(line.trim());
        saw_text = true;
        blanks = 0;
      }
      folded
    } else {
      stripped.join("\n")
    };
    // Clipping (`|`, `>`) keeps exactly one trailing line break; chomping (`|-`, `>-`)
    // keeps none.
    while text.ends_with('\n') {
      text.pop();
    }
    if self.keep_trailing && !text.is_empty() {
      text.push('\n');
    }
    text
  }
}

/// Store a block scalar that has finished collecting.
fn close_block(pending: &mut Option<(String, BlockScalar)>, fields: &mut BTreeMap<String, String>) {
  if let Some((key, block)) = pending.take() {
    fields.insert(key, block.value());
  }
}

/// Strip the quotes a YAML scalar may carry.
fn unquote(raw: &str) -> String {
  if let Some(body) = raw
    .strip_prefix('"')
    .and_then(|rest| rest.strip_suffix('"'))
  {
    let mut out = String::with_capacity(body.len());
    let mut escape = false;
    for ch in body.chars() {
      if escape {
        out.push(match ch {
          'n' => '\n',
          't' => '\t',
          other => other,
        });
        escape = false;
      } else if ch == '\\' {
        escape = true;
      } else {
        out.push(ch);
      }
    }
    return out;
  }
  if let Some(body) = raw
    .strip_prefix('\'')
    .and_then(|rest| rest.strip_suffix('\''))
  {
    return body.replace("''", "'");
  }
  raw.to_string()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn fields(text: &str) -> Frontmatter {
    match extract(text) {
      Extract::Found(frontmatter) => frontmatter,
      other => panic!("expected frontmatter, got {other:?}"),
    }
  }

  #[test]
  fn a_flat_block_becomes_pairs() {
    let frontmatter =
      fields("---\nname: pdf-processing\ndescription: Extracts text.\n---\n\n# Body\n");
    assert_eq!(frontmatter.get("name"), Some("pdf-processing"));
    assert_eq!(frontmatter.get("description"), Some("Extracts text."));
    assert_eq!(frontmatter.get("license"), None);
    assert_eq!(
      frontmatter.keys().collect::<Vec<_>>(),
      vec!["description", "name"]
    );
  }

  #[test]
  fn a_file_without_a_block_is_prose_not_a_broken_skill() {
    assert_eq!(
      extract("# My Skill\n\nname: not frontmatter\n"),
      Extract::Absent
    );
    // The fence has to be the first thing in the file, as in Pi.
    assert_eq!(extract("\n---\nname: late\n---\n"), Extract::Absent);
  }

  #[test]
  fn a_block_that_never_closes_is_reported_not_guessed() {
    assert_eq!(
      extract("---\nname: open\ndescription: no terminator\n"),
      Extract::Malformed(Malformed::Unterminated)
    );
  }

  #[test]
  fn quoted_values_keep_their_punctuation() {
    let frontmatter = fields(
      "---\ndescription: \"Use when the user says \\\"hf\\\", or 'huggingface'.\"\nname: 'quoted-name'\n---\n",
    );
    assert_eq!(
      frontmatter.get("description"),
      Some("Use when the user says \"hf\", or 'huggingface'.")
    );
    assert_eq!(frontmatter.get("name"), Some("quoted-name"));
  }

  #[test]
  fn a_folded_block_scalar_folds_line_breaks_and_keeps_blank_ones() {
    let frontmatter = fields(
      "---\nname: long\ndescription: >\n  Extracts text and tables from PDF files.\n  Fills forms, and merges documents.\n\n  Use for PDF work.\n---\n",
    );
    assert_eq!(
      frontmatter.get("description"),
      Some(
        "Extracts text and tables from PDF files. Fills forms, and merges documents.\nUse for PDF work.\n"
      )
    );
    // The whole paragraph is one line, and chomping removes the trailing break.
    let chomped = fields("---\nname: long\ndescription: >-\n  one\n  two\n---\n");
    assert_eq!(chomped.get("description"), Some("one two"));
  }

  #[test]
  fn a_literal_block_scalar_keeps_its_lines() {
    let frontmatter =
      fields("---\nname: literal\ncompatibility: |\n  needs python 3.12\n  needs network\n---\n");
    assert_eq!(
      frontmatter.get("compatibility"),
      Some("needs python 3.12\nneeds network\n")
    );
    let chomped = fields("---\nname: clipped\ncompatibility: |-\n  one line\n---\n");
    assert_eq!(chomped.get("compatibility"), Some("one line"));
  }

  #[test]
  fn comments_and_unmodelled_lines_are_skipped() {
    let frontmatter =
      fields("---\n# a comment\nname: ok\n  continuation: ignored\nlicense: MIT\n---\n");
    assert_eq!(frontmatter.get("name"), Some("ok"));
    assert_eq!(frontmatter.get("license"), Some("MIT"));
    assert_eq!(frontmatter.get("continuation"), None);
  }

  #[test]
  fn truth_is_spelled_true() {
    let frontmatter = fields("---\ndisable-model-invocation: true\n---\n");
    assert!(frontmatter.is_true("disable-model-invocation"));
    let other = fields("---\ndisable-model-invocation: yes\n---\n");
    assert!(
      !other.is_true("disable-model-invocation"),
      "a skill hidden on a spelling nobody wrote is a bug someone cannot see"
    );
  }

  #[test]
  fn crlf_line_endings_do_not_leak_into_values() {
    let frontmatter = fields("---\r\nname: crlf\r\ndescription: kept\r\n---\r\n");
    assert_eq!(frontmatter.get("name"), Some("crlf"));
    assert_eq!(frontmatter.get("description"), Some("kept"));
  }

  #[test]
  fn a_value_containing_a_hash_keeps_the_hash() {
    let frontmatter = fields("---\ndescription: Use # for tags\n---\n");
    assert_eq!(frontmatter.get("description"), Some("Use # for tags"));
  }

  #[test]
  fn an_empty_block_is_a_block() {
    assert_eq!(fields("---\n---").keys().count(), 0);
  }

  #[test]
  fn strip_removes_the_block_and_keeps_the_body() {
    assert_eq!(
      strip("---\nname: x\n---\nReview the diff.\n"),
      "Review the diff.\n"
    );
    // A closing line with no newline after it is still a closing line.
    assert_eq!(strip("---\nname: x\n---"), "");
    assert_eq!(strip("---\r\nname: x\r\n---\r\nbody\n"), "body\n");
    assert_eq!(strip("\u{feff}---\nname: x\n---\nbody"), "body");
  }

  #[test]
  fn strip_leaves_what_is_not_a_block_alone() {
    // Ordinary prose: there was never a block to take off.
    assert_eq!(strip("# Notes\n\nProse."), "# Notes\n\nProse.");
    // A block that never closed has no known end, so nothing can be honestly removed.
    assert_eq!(strip("---\nname: x\nbody"), "---\nname: x\nbody");
    // A `---` later in the document is a horizontal rule, not a header block.
    assert_eq!(
      strip("intro\n---\nname: x\n---\n"),
      "intro\n---\nname: x\n---\n"
    );
  }

  #[test]
  fn strip_does_not_swallow_a_horizontal_rule_in_the_body() {
    assert_eq!(
      strip("---\nname: x\n---\nbefore\n---\nafter\n"),
      "before\n---\nafter\n"
    );
  }
}
