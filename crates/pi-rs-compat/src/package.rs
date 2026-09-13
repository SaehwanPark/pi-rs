//! Parsing Pi-compatible package manifests (`package.json`).
//!
//! A Pi package is a directory with a `package.json` that may declare skills, prompt
//! templates, and TypeScript extensions. This module reads the manifest file, extracts
//! the surfaces pi-rs supports today, and produces explicit diagnostics for everything
//! it observed but cannot act on. Parsing is observation-only: nothing here becomes
//! runtime state, an event, or a capability unless the runtime deliberately adopts it.
//!
//! # What Pi packages declare
//!
//! A compatible `package.json` may carry:
//!
//! ```text
//! {
//!   "name": "my-package",
//!   "version": "1.0.0",
//!   "description": "...",
//!   "pi": {
//!     "skills":  ["skills/", "skills/pdf-tools"],
//!     "prompts": ["prompts/review.md"]
//!   },
//!   "extensions": ["dist/extension.js"]
//! }
//! ```
//!
//! The `pi.skills` and `pi.prompts` entries are directory or file paths relative to the
//! package root. A package may also have a `skills/` or `prompts/` sub-directory whose
//! files are discovered by convention without manifest entries; that discovery is the
//! responsibility of the caller, not this module. `extensions` are TypeScript/JavaScript
//! entry points that require the Node compatibility host, which is not started yet.
//!
//! # Surface support
//!
//! | Surface      | Support |
//! |--------------|---------|
//! | `pi.skills`  | paths extracted; callers integrate with skill discovery |
//! | `pi.prompts` | paths extracted; callers integrate with prompt discovery |
//! | `extensions` | recorded as [`Warning::UnsupportedSurface`]; Node host not started |
//! | Unknown Pi-namespace keys | recorded as [`Warning::UnknownSurface`] |
//!
//! Per-surface diagnostics let a caller report compatibility without refusing the whole
//! package because one optional feature is unrecognised.
//!
//! # JSON reading
//!
//! The reader handles the subset of JSON that appears in real Pi package manifests:
//! string scalars, arrays of strings, and a flat or one-level-deep object. It does not
//! attempt to model deeply nested objects, numbers, or booleans beyond detecting their
//! presence. This is the same philosophy as the frontmatter reader: read exactly what
//! the format documents, surface area for nothing beyond it.

use std::path::{Path, PathBuf};

/// The parsed content of a `package.json` that pi-rs can act on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
  /// The `name` field if present and a string.
  pub name: Option<String>,
  /// The `version` field if present and a string.
  pub version: Option<String>,
  /// The `description` field if present and a string.
  pub description: Option<String>,
  /// Paths declared in `pi.skills`, relative to the package directory. These point to
  /// skill directories or `SKILL.md` files and are integrated by the caller with the
  /// standard skill discovery rules.
  pub skill_paths: Vec<String>,
  /// Paths declared in `pi.prompts`, relative to the package directory. These point to
  /// prompt template files and are integrated by the caller with the standard prompt
  /// discovery rules.
  pub prompt_paths: Vec<String>,
}

/// A surface or condition worth reporting to the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
  /// `extensions` was declared. The Node compatibility host is not started by pi-rs
  /// automatically; TypeScript extensions will not run until Phase 8.
  UnsupportedSurface {
    /// Which surface name (`"extensions"`, etc.).
    surface: String,
    /// Number of entries that were declared (0 for a non-array value).
    count: usize,
  },
  /// A key inside the `pi` namespace that this reader does not know how to act on.
  /// Recorded so a reviewer can judge whether to add support without silently dropping
  /// the information.
  UnknownSurface { key: String, value_kind: ValueKind },
  /// The `package.json` file could not be read (permissions, not UTF-8, etc.).
  Unreadable { path: PathBuf, reason: String },
  /// The file contains text that is not valid JSON according to this reader's model.
  /// The message names the position or structural reason.
  Malformed { path: PathBuf, reason: String },
}

/// A coarse classification of a JSON value, used in unknown-surface diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
  String,
  Array,
  Object,
  Number,
  Bool,
  Null,
  Unknown,
}

impl ValueKind {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::String => "string",
      Self::Array => "array",
      Self::Object => "object",
      Self::Number => "number",
      Self::Bool => "boolean",
      Self::Null => "null",
      Self::Unknown => "unknown",
    }
  }
}

/// The result of parsing a `package.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parse {
  pub manifest: Manifest,
  pub warnings: Vec<Warning>,
}

/// Read a `package.json` file at `path` and return what could be extracted.
///
/// An unreadable or unparseable file produces a `Parse` with no manifest data and a
/// single warning explaining what happened; the caller can treat that the same as an
/// empty manifest with a diagnostic, which is the honest representation of "we tried".
pub fn read(path: &Path) -> Parse {
  let text = match std::fs::read_to_string(path) {
    Ok(text) => text,
    Err(error) => {
      return Parse {
        warnings: vec![Warning::Unreadable {
          path: path.to_path_buf(),
          reason: error.to_string(),
        }],
        ..Default::default()
      };
    }
  };
  parse_text(&text, path)
}

/// Parse the text of a `package.json`. Exposed separately so tests can pass strings
/// directly without touching the filesystem.
pub fn parse_text(text: &str, path: &Path) -> Parse {
  let mut manifest = Manifest::default();
  let mut warnings: Vec<Warning> = Vec::new();

  let mut parser = Parser::new(text);
  match parser.parse_top_level(&mut manifest, &mut warnings) {
    Ok(()) => {}
    Err(reason) => {
      warnings.push(Warning::Malformed {
        path: path.to_path_buf(),
        reason,
      });
    }
  }

  Parse { manifest, warnings }
}

// ---------------------------------------------------------------------------
// Minimal hand-rolled JSON reader
// ---------------------------------------------------------------------------
//
// The goals are identical to those of the frontmatter reader: parse exactly
// what Pi package.json manifests use, nothing more. The grammar handled:
//
//   object    ::= '{' (pair (',' pair)*)? '}'
//   pair      ::= string ':' value
//   value     ::= string | array | object | literal
//   string    ::= '"' char* '"'  (\n \t \\ \" recognised; \uXXXX decoded)
//   array     ::= '[' (value (',' value)*)? ']'
//   literal   ::= 'true' | 'false' | 'null' | [-0-9.]+
//
// Comments, trailing commas, single-quoted strings, and multi-document
// structures are not JSON and are not handled.

struct Parser<'a> {
  src: &'a [u8],
  pos: usize,
}

impl<'a> Parser<'a> {
  fn new(text: &'a str) -> Self {
    Self {
      src: text.as_bytes(),
      pos: 0,
    }
  }

  fn peek(&self) -> Option<u8> {
    self.src.get(self.pos).copied()
  }

  fn advance(&mut self) {
    self.pos += 1;
  }

  fn skip_whitespace(&mut self) {
    while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.peek() {
      self.advance();
    }
  }

  fn expect_byte(&mut self, byte: u8) -> Result<(), String> {
    self.skip_whitespace();
    match self.peek() {
      Some(b) if b == byte => {
        self.advance();
        Ok(())
      }
      Some(b) => Err(format!(
        "expected '{}' at position {}, found '{}'",
        byte as char, self.pos, b as char
      )),
      None => Err(format!(
        "unexpected end of input, expected '{}'",
        byte as char
      )),
    }
  }

  /// Read a JSON string value (the surrounding quotes are consumed).
  fn parse_string(&mut self) -> Result<String, String> {
    self.skip_whitespace();
    match self.peek() {
      Some(b'"') => self.advance(),
      Some(b) => {
        return Err(format!(
          "expected '\"' at position {}, found '{}'",
          self.pos, b as char
        ));
      }
      None => return Err("unexpected end of input, expected '\"'".to_string()),
    }
    let mut out = String::new();
    loop {
      match self.peek() {
        None => return Err("unterminated string".to_string()),
        Some(b'"') => {
          self.advance();
          return Ok(out);
        }
        Some(b'\\') => {
          self.advance();
          match self.peek() {
            None => return Err("unterminated escape".to_string()),
            Some(b'"') => {
              out.push('"');
              self.advance();
            }
            Some(b'\\') => {
              out.push('\\');
              self.advance();
            }
            Some(b'n') => {
              out.push('\n');
              self.advance();
            }
            Some(b't') => {
              out.push('\t');
              self.advance();
            }
            Some(b'r') => {
              out.push('\r');
              self.advance();
            }
            Some(b'/') => {
              out.push('/');
              self.advance();
            }
            Some(b'b') => {
              out.push('\x08');
              self.advance();
            }
            Some(b'f') => {
              out.push('\x0C');
              self.advance();
            }
            Some(b'u') => {
              // 4 hex digits
              self.advance();
              let start = self.pos;
              for _ in 0..4 {
                match self.peek() {
                  Some(b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F') => self.advance(),
                  _ => return Err(format!("invalid unicode escape at position {}", self.pos)),
                }
              }
              let hex = std::str::from_utf8(&self.src[start..self.pos]).expect("ascii hex digits");
              let code_point = u32::from_str_radix(hex, 16)
                .map_err(|_| format!("invalid unicode escape '\\u{hex}'"))?;
              let ch = char::from_u32(code_point)
                .ok_or_else(|| format!("unrepresentable code point U+{code_point:04X}"))?;
              out.push(ch);
            }
            Some(other) => {
              // Unknown escape: pass through verbatim (lenient).
              out.push(other as char);
              self.advance();
            }
          }
        }
        Some(b) => {
          // Decode a UTF-8 character from the byte slice.
          let start = self.pos;
          let width = utf8_char_width(b);
          let end = start + width;
          if end > self.src.len() {
            return Err(format!("truncated UTF-8 sequence at position {start}"));
          }
          let s = std::str::from_utf8(&self.src[start..end])
            .map_err(|_| format!("invalid UTF-8 at position {start}"))?
            .to_string();
          out.push_str(&s);
          self.pos = end;
        }
      }
    }
  }

  /// Parse an array of strings, collecting string items and skipping non-string ones.
  fn parse_string_array(&mut self) -> Result<Vec<String>, String> {
    self.expect_byte(b'[')?;
    self.skip_whitespace();
    let mut items = Vec::new();
    if self.peek() == Some(b']') {
      self.advance();
      return Ok(items);
    }
    loop {
      self.skip_whitespace();
      if self.peek() == Some(b'"') {
        items.push(self.parse_string()?);
      } else {
        self.skip_value()?;
      }
      self.skip_whitespace();
      match self.peek() {
        Some(b',') => {
          self.advance();
        }
        Some(b']') => {
          self.advance();
          return Ok(items);
        }
        Some(b) => {
          return Err(format!(
            "expected ',' or ']' at position {}, found '{}'",
            self.pos, b as char
          ));
        }
        None => return Err("unterminated array".to_string()),
      }
    }
  }

  /// Skip over one JSON value (string, array, object, or literal) without retaining
  /// its content. Used to step past values in keys this reader does not recognise.
  fn skip_value(&mut self) -> Result<(), String> {
    self.skip_whitespace();
    match self.peek() {
      Some(b'"') => {
        self.parse_string()?;
      }
      Some(b'[') => {
        self.advance();
        let mut depth = 1usize;
        while depth > 0 {
          match self.peek() {
            None => return Err("unterminated array in skip".to_string()),
            Some(b'[') => {
              depth += 1;
              self.advance();
            }
            Some(b']') => {
              depth -= 1;
              self.advance();
            }
            Some(b'"') => {
              self.parse_string()?;
            }
            _ => self.advance(),
          }
        }
      }
      Some(b'{') => {
        self.advance();
        let mut depth = 1usize;
        while depth > 0 {
          match self.peek() {
            None => return Err("unterminated object in skip".to_string()),
            Some(b'{') => {
              depth += 1;
              self.advance();
            }
            Some(b'}') => {
              depth -= 1;
              self.advance();
            }
            Some(b'"') => {
              self.parse_string()?;
            }
            _ => self.advance(),
          }
        }
      }
      Some(_) => {
        // Literal: true, false, null, or a number. Read until delimiter.
        while let Some(b) = self.peek() {
          match b {
            b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r' => break,
            _ => self.advance(),
          }
        }
      }
      None => return Err("unexpected end of input in value".to_string()),
    }
    Ok(())
  }

  /// Classify a value without consuming it: peek at the first non-whitespace byte.
  fn value_kind(&self) -> ValueKind {
    let mut i = self.pos;
    while i < self.src.len() {
      match self.src[i] {
        b' ' | b'\t' | b'\n' | b'\r' => i += 1,
        b'"' => return ValueKind::String,
        b'[' => return ValueKind::Array,
        b'{' => return ValueKind::Object,
        b't' | b'f' => return ValueKind::Bool,
        b'n' => return ValueKind::Null,
        b'0'..=b'9' | b'-' => return ValueKind::Number,
        _ => return ValueKind::Unknown,
      }
    }
    ValueKind::Unknown
  }

  /// Count array entries without retaining them, for the UnsupportedSurface count.
  fn count_array_entries(&mut self) -> Result<usize, String> {
    self.expect_byte(b'[')?;
    self.skip_whitespace();
    if self.peek() == Some(b']') {
      self.advance();
      return Ok(0);
    }
    let mut count = 0usize;
    loop {
      self.skip_value()?;
      count += 1;
      self.skip_whitespace();
      match self.peek() {
        Some(b',') => self.advance(),
        Some(b']') => {
          self.advance();
          return Ok(count);
        }
        Some(b) => {
          return Err(format!(
            "expected ',' or ']' at position {}, found '{}'",
            self.pos, b as char
          ));
        }
        None => return Err("unterminated array".to_string()),
      }
    }
  }

  /// Parse the `pi` namespace object: `{ "skills": [...], "prompts": [...], ... }`.
  fn parse_pi_namespace(
    &mut self,
    manifest: &mut Manifest,
    warnings: &mut Vec<Warning>,
  ) -> Result<(), String> {
    self.expect_byte(b'{')?;
    self.skip_whitespace();
    if self.peek() == Some(b'}') {
      self.advance();
      return Ok(());
    }
    loop {
      self.skip_whitespace();
      let key = self.parse_string()?;
      self.skip_whitespace();
      self.expect_byte(b':')?;
      match key.as_str() {
        "skills" => {
          manifest.skill_paths = self.parse_string_array()?;
        }
        "prompts" => {
          manifest.prompt_paths = self.parse_string_array()?;
        }
        _ => {
          // Unknown key within the pi namespace: record its type and skip.
          let kind = self.value_kind();
          warnings.push(Warning::UnknownSurface {
            key,
            value_kind: kind,
          });
          self.skip_value()?;
        }
      }
      self.skip_whitespace();
      match self.peek() {
        Some(b',') => {
          self.advance();
        }
        Some(b'}') => {
          self.advance();
          return Ok(());
        }
        Some(b) => {
          return Err(format!(
            "expected ',' or '}}' at position {}, found '{}'",
            self.pos, b as char
          ));
        }
        None => return Err("unterminated object".to_string()),
      }
    }
  }

  /// Parse the top-level `package.json` object.
  fn parse_top_level(
    &mut self,
    manifest: &mut Manifest,
    warnings: &mut Vec<Warning>,
  ) -> Result<(), String> {
    self.skip_whitespace();
    self.expect_byte(b'{')?;
    self.skip_whitespace();
    if self.peek() == Some(b'}') {
      self.advance();
      return Ok(());
    }
    loop {
      self.skip_whitespace();
      let key = self.parse_string()?;
      self.skip_whitespace();
      self.expect_byte(b':')?;

      match key.as_str() {
        "name" => {
          manifest.name = Some(self.parse_string()?);
        }
        "version" => {
          manifest.version = Some(self.parse_string()?);
        }
        "description" => {
          manifest.description = Some(self.parse_string()?);
        }
        "pi" => {
          // The Pi-namespace object: contains "skills", "prompts", and future surfaces.
          self.parse_pi_namespace(manifest, warnings)?;
        }
        "extensions" => {
          // Node/TypeScript extension entry points: not supported until Phase 8.
          let kind = self.value_kind();
          let count = if kind == ValueKind::Array {
            self.count_array_entries()?
          } else {
            self.skip_value()?;
            0
          };
          warnings.push(Warning::UnsupportedSurface {
            surface: "extensions".to_string(),
            count,
          });
        }
        _ => {
          // Standard npm metadata keys (main, license, author, keywords, repository,
          // scripts, dependencies, devDependencies, …) are not Pi surfaces and are
          // silently skipped so they do not pollute the diagnostic output with noise.
          self.skip_value()?;
        }
      }

      self.skip_whitespace();
      match self.peek() {
        Some(b',') => {
          self.advance();
        }
        Some(b'}') => {
          self.advance();
          return Ok(());
        }
        Some(b) => {
          return Err(format!(
            "expected ',' or '}}' at position {}, found '{}'",
            self.pos, b as char
          ));
        }
        None => return Err("unterminated object".to_string()),
      }
    }
  }
}

/// Width of a UTF-8 character from its leading byte.
fn utf8_char_width(b: u8) -> usize {
  match b {
    0x00..=0x7F => 1,
    0xC0..=0xDF => 2,
    0xE0..=0xEF => 3,
    0xF0..=0xF7 => 4,
    _ => 1, // continuation byte or invalid: treat as one byte
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn path() -> &'static Path {
    Path::new("package.json")
  }

  fn parse(text: &str) -> Parse {
    parse_text(text, path())
  }

  fn manifest(text: &str) -> Manifest {
    let p = parse(text);
    assert!(
      p.warnings.is_empty(),
      "unexpected warnings: {:?}",
      p.warnings
    );
    p.manifest
  }

  // -------------------------------------------------------------------------
  // Happy-path manifest fields
  // -------------------------------------------------------------------------

  #[test]
  fn an_empty_object_produces_an_empty_manifest() {
    let m = manifest("{}");
    assert_eq!(m.name, None);
    assert_eq!(m.version, None);
    assert_eq!(m.description, None);
    assert!(m.skill_paths.is_empty());
    assert!(m.prompt_paths.is_empty());
  }

  #[test]
  fn name_version_and_description_are_extracted() {
    let m = manifest(
      r#"{
        "name": "my-package",
        "version": "1.2.3",
        "description": "Extracts text from PDFs."
      }"#,
    );
    assert_eq!(m.name.as_deref(), Some("my-package"));
    assert_eq!(m.version.as_deref(), Some("1.2.3"));
    assert_eq!(m.description.as_deref(), Some("Extracts text from PDFs."));
  }

  #[test]
  fn pi_skills_array_is_extracted_as_relative_paths() {
    let m = manifest(
      r#"{
        "name": "pkg",
        "pi": {
          "skills": ["skills/", "skills/pdf-tools", "SKILL.md"]
        }
      }"#,
    );
    assert_eq!(m.skill_paths, ["skills/", "skills/pdf-tools", "SKILL.md"]);
  }

  #[test]
  fn pi_prompts_array_is_extracted_as_relative_paths() {
    let m = manifest(
      r#"{
        "name": "pkg",
        "pi": {
          "prompts": ["prompts/review.md", "prompts/plan.md"]
        }
      }"#,
    );
    assert_eq!(m.prompt_paths, ["prompts/review.md", "prompts/plan.md"]);
  }

  #[test]
  fn empty_pi_skills_and_prompts_arrays_are_fine() {
    let m = manifest(r#"{"name":"pkg","pi":{"skills":[],"prompts":[]}}"#);
    assert!(m.skill_paths.is_empty());
    assert!(m.prompt_paths.is_empty());
  }

  #[test]
  fn empty_pi_namespace_is_fine() {
    let m = manifest(r#"{"name":"pkg","pi":{}}"#);
    assert!(m.skill_paths.is_empty());
    assert!(m.prompt_paths.is_empty());
    assert_eq!(m.name.as_deref(), Some("pkg"));
  }

  // -------------------------------------------------------------------------
  // Unsupported surfaces produce warnings, not errors
  // -------------------------------------------------------------------------

  #[test]
  fn extensions_produces_an_unsupported_surface_warning() {
    let p = parse(
      r#"{
        "name": "pkg",
        "extensions": ["dist/main.js", "dist/tools.js"]
      }"#,
    );
    assert_eq!(p.manifest.name.as_deref(), Some("pkg"));
    assert_eq!(p.warnings.len(), 1);
    assert!(matches!(
      &p.warnings[0],
      Warning::UnsupportedSurface { surface, count }
        if surface == "extensions" && *count == 2
    ));
  }

  #[test]
  fn extensions_count_is_zero_for_an_empty_array() {
    let p = parse(r#"{"name":"pkg","extensions":[]}"#);
    assert!(matches!(
      &p.warnings[0],
      Warning::UnsupportedSurface { count, .. } if *count == 0
    ));
  }

  #[test]
  fn unknown_pi_namespace_key_produces_unknown_surface_warning() {
    let p = parse(r#"{"name":"pkg","pi":{"skills":["s/"],"themes":["dark.json"]}}"#);
    assert_eq!(p.manifest.skill_paths, ["s/"]);
    assert!(matches!(
      &p.warnings[0],
      Warning::UnknownSurface { key, value_kind: ValueKind::Array }
        if key == "themes"
    ));
  }

  // -------------------------------------------------------------------------
  // Benign npm metadata does not produce noise
  // -------------------------------------------------------------------------

  #[test]
  fn standard_npm_keys_are_silently_skipped() {
    let p = parse(
      r#"{
        "name": "pkg",
        "version": "1.0.0",
        "license": "MIT",
        "author": "Alice",
        "main": "dist/index.js",
        "keywords": ["coding", "agent"],
        "repository": {"type": "git", "url": "https://example.com/repo"},
        "dependencies": {},
        "devDependencies": {}
      }"#,
    );
    // No warnings for pure npm metadata.
    assert!(
      p.warnings.is_empty(),
      "unexpected warnings: {:?}",
      p.warnings
    );
    assert_eq!(p.manifest.name.as_deref(), Some("pkg"));
    assert_eq!(p.manifest.version.as_deref(), Some("1.0.0"));
  }

  // -------------------------------------------------------------------------
  // String escapes
  // -------------------------------------------------------------------------

  #[test]
  fn string_escape_sequences_are_unescaped() {
    let m = manifest(r#"{"name":"a\/b","description":"line1\nline2"}"#);
    assert_eq!(m.name.as_deref(), Some("a/b"));
    assert_eq!(m.description.as_deref(), Some("line1\nline2"));
  }

  #[test]
  fn unicode_escape_in_string_is_decoded() {
    let m = manifest(r#"{"name":"\u0070kg"}"#);
    assert_eq!(m.name.as_deref(), Some("pkg"));
  }

  // -------------------------------------------------------------------------
  // Error cases
  // -------------------------------------------------------------------------

  #[test]
  fn not_a_json_object_is_malformed() {
    let p = parse("[]");
    assert!(matches!(p.warnings.as_slice(), [Warning::Malformed { .. }]));
  }

  #[test]
  fn truncated_object_is_malformed() {
    let p = parse(r#"{"name": "x""#);
    assert!(matches!(p.warnings.as_slice(), [Warning::Malformed { .. }]));
  }

  #[test]
  fn unterminated_string_value_is_malformed() {
    let p = parse(r#"{"name": "unclosed}"#);
    assert!(matches!(p.warnings.as_slice(), [Warning::Malformed { .. }]));
  }

  #[test]
  fn completely_empty_input_is_malformed() {
    let p = parse("");
    assert!(matches!(p.warnings.as_slice(), [Warning::Malformed { .. }]));
  }

  // -------------------------------------------------------------------------
  // Mix of supported and unsupported surfaces in one manifest
  // -------------------------------------------------------------------------

  #[test]
  fn a_full_manifest_with_extensions_extracts_what_it_can() {
    let p = parse(
      r#"{
        "name": "complete-pkg",
        "version": "2.0.0",
        "description": "A complete package.",
        "pi": {
          "skills":  ["skills/"],
          "prompts": ["prompts/review.md"]
        },
        "extensions": ["dist/ext.js"]
      }"#,
    );
    assert_eq!(p.manifest.name.as_deref(), Some("complete-pkg"));
    assert_eq!(p.manifest.skill_paths, ["skills/"]);
    assert_eq!(p.manifest.prompt_paths, ["prompts/review.md"]);
    assert_eq!(p.warnings.len(), 1);
    assert!(matches!(
      &p.warnings[0],
      Warning::UnsupportedSurface { surface, count }
        if surface == "extensions" && *count == 1
    ));
  }

  // -------------------------------------------------------------------------
  // Whitespace and formatting variations
  // -------------------------------------------------------------------------

  #[test]
  fn compact_json_with_no_whitespace_is_parsed() {
    let m =
      manifest(r#"{"name":"pkg","version":"1.0.0","pi":{"skills":["s/"],"prompts":["p.md"]}}"#);
    assert_eq!(m.name.as_deref(), Some("pkg"));
    assert_eq!(m.skill_paths, ["s/"]);
    assert_eq!(m.prompt_paths, ["p.md"]);
  }

  #[test]
  fn deeply_indented_json_is_parsed() {
    let m = manifest(
      "{\n  \"name\": \"indented\",\n  \"pi\": {\n    \"skills\": [\n      \"skills/\"\n    ]\n  }\n}",
    );
    assert_eq!(m.name.as_deref(), Some("indented"));
    assert_eq!(m.skill_paths, ["skills/"]);
  }
}
