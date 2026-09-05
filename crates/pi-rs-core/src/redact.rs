//! Redaction boundary.
//!
//! Trace data may contain secrets: environment values leak into shell output,
//! provider payloads echo headers, and project content is sensitive by
//! default. The rule is structural — durable output goes through a policy
//! before it is written — so that forgetting to redact is not the default.
//!
//! Redaction is best-effort noise removal, not a boundary against an adversary
//! who controls the text. Two properties are required instead:
//!
//! - **Deterministic.** Same input and policy, same output, so journals can be
//!   diffed and replayed.
//! - **Counted.** The number of replacements is returned, so a persisted line
//!   can state that it was sanitized.
//!
//! No regex crate is introduced for this: the patterns are token-shaped, and a
//! bounded token scan is cheaper on the startup path and easier to audit.

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

/// Replacement marker. Kind-labeled forms are produced by the private `marker` helper.
pub const MARKER: &str = "[redacted]";

/// Which class of secret a replacement came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretKind {
  /// Value of an environment variable.
  Env,
  /// Provider key recognized by its public prefix.
  ProviderKey,
  /// `Bearer`/`Basic` style credential.
  Authorization,
  /// `scheme://user:password@host` credentials.
  UrlCredentials,
  /// Private key block.
  PrivateKey,
  /// Value of a JSON key that is known to carry secrets.
  SecretField,
}

impl SecretKind {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Env => "env",
      Self::ProviderKey => "key",
      Self::Authorization => "auth",
      Self::UrlCredentials => "url",
      Self::PrivateKey => "pkey",
      Self::SecretField => "field",
    }
  }
}

/// `[redacted:kind]`.
fn marker(kind: SecretKind) -> String {
  format!("[redacted:{}]", kind.as_str())
}

/// Result of one redaction pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redacted {
  pub text: String,
  pub replacements: u32,
}

impl Redacted {
  fn unchanged(text: &str) -> Self {
    Self {
      text: text.to_string(),
      replacements: 0,
    }
  }
}

/// Redaction policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactionPolicy {
  /// `false` disables redaction. Disabling is an explicit config choice, never
  /// inferred, because it has a security consequence.
  pub enabled: bool,
  /// Environment values shorter than this are ignored, so that short tokens do
  /// not mangle ordinary prose.
  pub min_secret_len: usize,
  /// Additional literal values to redact, for example a project token.
  #[serde(default)]
  pub literals: Vec<String>,
  /// Scan for process environment values that look like credentials.
  pub scan_environment: bool,
}

impl Default for RedactionPolicy {
  fn default() -> Self {
    Self {
      enabled: true,
      min_secret_len: 8,
      literals: Vec::new(),
      scan_environment: true,
    }
  }
}

/// Provider key prefixes with the minimum trailing length that makes a match
/// unambiguous. Sorted longest-first at match time.
const KEY_PREFIXES: &[(&str, usize)] = &[
  ("sk-ant-", 12),
  ("github_pat_", 12),
  ("ghp_", 12),
  ("gho_", 12),
  ("ghs_", 12),
  ("glpat-", 12),
  ("dckr_pat_", 12),
  ("ya29.", 8),
  ("AKIA", 8),
  ("AIza", 8),
  ("sk-", 12),
  ("xox", 10),
  ("npm_", 12),
  ("pypi-", 12),
  ("hf_", 12),
  ("xai-", 12),
  ("sgkey-", 12),
];

/// JSON keys whose values are credentials rather than prose.
const SECRET_FIELDS: &[&str] = &[
  "api_key",
  "apikey",
  "api_secret",
  "authorization",
  "access_token",
  "refresh_token",
  "id_token",
  "client_secret",
  "password",
  "secret",
  "token",
  "x-api-key",
];

impl RedactionPolicy {
  /// Default policy, which scans the process environment.
  pub fn from_env() -> Self {
    Self::default()
  }

  /// Redact one string.
  pub fn apply(&self, input: &str) -> Redacted {
    if !self.enabled {
      return Redacted::unchanged(input);
    }
    let mut replacements = 0u32;
    let mut text = input.to_string();

    for (secret, kind) in self.candidates() {
      let hits = text.matches(secret.as_str()).count();
      if hits > 0 {
        text = text.replace(secret.as_str(), &marker(kind));
        replacements += hits as u32;
      }
    }

    text = redact_private_keys(&text, &mut replacements);
    text = redact_url_credentials(&text, &mut replacements);
    text = redact_authorization(&text, &mut replacements);
    text = redact_key_prefixes(&text, &mut replacements, self.min_secret_len);

    Redacted { text, replacements }
  }

  /// Redact a JSON value in place and report how many replacements were made.
  ///
  /// Keys that are known to carry credentials are replaced wholesale; other
  /// strings are scanned. That way a token inside prose is caught while a
  /// structured credential is never partially preserved.
  pub fn apply_json(&self, value: &mut serde_json::Value) -> u32 {
    if !self.enabled {
      return 0;
    }
    let mut replacements = 0u32;
    self.walk_json(value, &mut replacements);
    replacements
  }

  fn walk_json(&self, value: &mut serde_json::Value, replacements: &mut u32) {
    match value {
      serde_json::Value::String(text) => {
        let redacted = self.apply(text);
        *replacements += redacted.replacements;
        *text = redacted.text;
      }
      serde_json::Value::Array(items) => {
        for item in items {
          self.walk_json(item, replacements);
        }
      }
      serde_json::Value::Object(map) => {
        let mut credentials = Vec::new();
        for (key, item) in map.iter_mut() {
          if is_secret_field(key) {
            if !item.is_null() {
              credentials.push(key.clone());
            }
            continue;
          }
          self.walk_json(item, replacements);
        }
        for key in credentials {
          *replacements += 1;
          map.insert(
            key,
            serde_json::Value::String(marker(SecretKind::SecretField)),
          );
        }
      }
      _ => {}
    }
  }

  /// Literal candidates, longest first so that a short secret which is a
  /// substring of a longer one cannot leave a fragment of the longer one.
  fn candidates(&self) -> Vec<(String, SecretKind)> {
    let mut out = Vec::new();
    for literal in &self.literals {
      if literal.chars().count() >= self.min_secret_len {
        out.push((literal.clone(), SecretKind::SecretField));
      }
    }
    if self.scan_environment {
      for (value, _name) in env_secrets() {
        if value.chars().count() >= self.min_secret_len {
          out.push((value.clone(), SecretKind::Env));
        }
      }
    }
    out.sort_by_key(|entry| std::cmp::Reverse(entry.0.chars().count()));
    out
  }
}

fn is_secret_field(key: &str) -> bool {
  let lowered = key.to_ascii_lowercase();
  SECRET_FIELDS.contains(&lowered.as_str())
    || lowered.ends_with("_api_key")
    || lowered.ends_with("_token")
    || lowered.ends_with("_secret")
}

/// Environment values worth scanning for, snapshotted once per process.
///
/// Reading the environment per event would be steady-state waste, and a stable
/// snapshot keeps redaction deterministic within a process.
fn env_secrets() -> &'static [(String, String)] {
  static CACHE: OnceLock<Vec<(String, String)>> = OnceLock::new();
  CACHE.get_or_init(|| {
    let mut secrets = Vec::new();
    for (name, value) in std::env::vars() {
      if looks_like_secret_name(&name) && !looks_like_endpoint(&value) && value.len() >= 8 {
        secrets.push((value, name));
      }
    }
    secrets.sort_by_key(|entry| std::cmp::Reverse(entry.0.len()));
    secrets
  })
}

fn looks_like_secret_name(name: &str) -> bool {
  let lowered = name.to_ascii_lowercase();
  [
    "key",
    "token",
    "secret",
    "password",
    "pass",
    "credential",
    "auth",
    "cookie",
  ]
  .iter()
  .any(|needle| lowered.contains(needle))
}

fn looks_like_endpoint(value: &str) -> bool {
  value.starts_with("http://")
    || value.starts_with("https://")
    || value.starts_with("unix://")
    || value.starts_with('/')
    || value.starts_with('.')
}

fn is_token_char(ch: char) -> bool {
  ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '+' | '/' | '.' | ':')
}

/// Replace private key blocks line-wise, from `-----BEGIN ... PRIVATE KEY-----`
/// through the matching END line.
fn redact_private_keys(text: &str, replacements: &mut u32) -> String {
  let mut out = String::with_capacity(text.len());
  let mut skipping = false;
  for line in text.split_inclusive('\n') {
    let trimmed = line.trim();
    if !skipping && trimmed.starts_with("-----BEGIN") && trimmed.contains("PRIVATE KEY") {
      skipping = true;
      out.push_str(&marker(SecretKind::PrivateKey));
      out.push('\n');
      continue;
    }
    if skipping {
      if trimmed.contains("-----END") {
        skipping = false;
        *replacements += 1;
      }
      continue;
    }
    out.push_str(line);
  }
  if skipping {
    // Unterminated block: the content was still withheld.
    *replacements += 1;
  }
  out
}

/// Replace the credential part of `scheme://authority@host`.
fn redact_url_credentials(text: &str, replacements: &mut u32) -> String {
  let mut out = String::with_capacity(text.len());
  let mut rest = text;
  while let Some(scheme_at) = rest.find("://") {
    let after = &rest[scheme_at + 3..];
    let Some(at) = after.find('@') else {
      break;
    };
    // A newline before the `@` means this is prose, not a URL authority.
    if after[..at].contains('\n') || after[..at].contains(' ') {
      out.push_str(&rest[..scheme_at + 3]);
      rest = after;
      continue;
    }
    out.push_str(&rest[..scheme_at + 3]);
    out.push_str(&marker(SecretKind::UrlCredentials));
    out.push('@');
    rest = &after[at + 1..];
    *replacements += 1;
  }
  out.push_str(rest);
  out
}

/// Replace `Bearer <token>` and `Basic <credentials>`.
fn redact_authorization(text: &str, replacements: &mut u32) -> String {
  let mut out = String::with_capacity(text.len());
  let mut rest = text;
  while let Some((index, scheme_len)) = find_scheme(rest) {
    let after_scheme = &rest[index + scheme_len..];
    let separator_len = after_scheme
      .chars()
      .next()
      .filter(|ch| ch.is_whitespace() || *ch == ':' || *ch == '=')
      .map(|ch| ch.len_utf8())
      .unwrap_or(0);
    if separator_len == 0 {
      // `basic`/`bearer` used as an ordinary word.
      out.push_str(&rest[..index + scheme_len]);
      rest = after_scheme;
      continue;
    }
    let value_start = index + scheme_len + separator_len;
    let value: String = rest[value_start..]
      .chars()
      .take_while(|ch| !ch.is_whitespace() && *ch != '"' && *ch != '\'' && *ch != ',')
      .collect();
    if value.chars().count() < 4 {
      out.push_str(&rest[..value_start]);
      rest = &rest[value_start..];
      continue;
    }
    out.push_str(&rest[..value_start]);
    out.push_str(&marker(SecretKind::Authorization));
    rest = &rest[value_start + value.len()..];
    *replacements += 1;
  }
  out.push_str(rest);
  out
}

/// Find the next `bearer`/`basic` scheme, case-insensitively, at a token
/// boundary. Returns byte offset and scheme length.
fn find_scheme(text: &str) -> Option<(usize, usize)> {
  let bytes = text.as_bytes();
  for index in 0..bytes.len() {
    if !text.is_char_boundary(index) {
      continue;
    }
    // A non-boundary byte before `index` belongs to a multi-byte character,
    // which is never an ASCII token character, so it is a boundary.
    let at_boundary =
      index == 0 || !text.is_char_boundary(index - 1) || !is_token_char(bytes[index - 1] as char);
    if !at_boundary {
      continue;
    }
    if index + 6 <= bytes.len()
      && text.is_char_boundary(index + 6)
      && text[index..index + 6].eq_ignore_ascii_case("bearer")
    {
      return Some((index, 6));
    }
    if index + 5 <= bytes.len()
      && text.is_char_boundary(index + 5)
      && text[index..index + 5].eq_ignore_ascii_case("basic")
    {
      return Some((index, 5));
    }
  }
  None
}

/// Replace provider keys recognized by prefix.
fn redact_key_prefixes(text: &str, replacements: &mut u32, min_secret_len: usize) -> String {
  let mut out = String::with_capacity(text.len());
  let mut token = String::new();
  let flush = |token: &mut String, out: &mut String, replacements: &mut u32| {
    if token.is_empty() {
      return;
    }
    if let Some(prefix) = matching_prefix(token, min_secret_len) {
      out.push_str(&format!("[redacted:key {prefix}]"));
      *replacements += 1;
    } else {
      out.push_str(token);
    }
    token.clear();
  };

  for ch in text.chars() {
    if is_token_char(ch) {
      token.push(ch);
      continue;
    }
    flush(&mut token, &mut out, replacements);
    out.push(ch);
  }
  flush(&mut token, &mut out, replacements);
  out
}

fn matching_prefix(token: &str, min_secret_len: usize) -> Option<&'static str> {
  KEY_PREFIXES.iter().find_map(|(prefix, min_tail)| {
    let tail = token.len().saturating_sub(prefix.len());
    if token.starts_with(prefix) && (*min_tail).max(min_secret_len) <= tail {
      Some(*prefix)
    } else {
      None
    }
  })
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

  fn policy() -> RedactionPolicy {
    RedactionPolicy {
      enabled: true,
      min_secret_len: 8,
      literals: vec!["project-token-abcdef123456".into()],
      scan_environment: false,
    }
  }

  #[test]
  fn literal_secrets_are_replaced_and_counted() {
    let redacted = policy().apply("export TOKEN=project-token-abcdef123456 # deploy");
    assert_eq!(redacted.replacements, 1);
    assert!(
      redacted.text.contains("[redacted:field]"),
      "{}",
      redacted.text
    );
    assert!(
      !redacted.text.contains("project-token-abcdef"),
      "secret must not survive: {}",
      redacted.text
    );
  }

  #[test]
  fn provider_keys_are_replaced_by_class() {
    let redacted =
      policy().apply("using sk-abcdefghijklmnopqrstuvwxyz and ghp_abcdefghijklmnopqrstuvwxyz");
    assert!(
      redacted.text.contains("[redacted:key sk-]"),
      "{}",
      redacted.text
    );
    assert!(
      redacted.text.contains("[redacted:key ghp_]"),
      "{}",
      redacted.text
    );
    assert!(
      !redacted.text.contains("sk-abcdefghij"),
      "{}",
      redacted.text
    );
    assert_eq!(redacted.replacements, 2);
  }

  #[test]
  fn prefix_like_words_survive() {
    let redacted = policy().apply("the key-value pair and sk-short and bearer of the package");
    assert_eq!(redacted.replacements, 0, "{}", redacted.text);
    assert_eq!(
      redacted.text,
      "the key-value pair and sk-short and bearer of the package"
    );
  }

  #[test]
  fn bearer_tokens_are_removed() {
    let redacted = policy().apply("header Authorization: Bearer abcdef123456789 next");
    assert!(
      redacted.text.contains("[redacted:auth]"),
      "{}",
      redacted.text
    );
    assert!(
      !redacted.text.contains("abcdef123456789"),
      "{}",
      redacted.text
    );
    assert_eq!(redacted.replacements, 1);
  }

  #[test]
  fn url_passwords_are_removed() {
    let redacted = policy().apply("clone https://user:hunter2@git.example.com/repo.git now");
    assert!(!redacted.text.contains("hunter2"), "{}", redacted.text);
    assert!(
      redacted
        .text
        .contains("https://[redacted:url]@git.example.com/repo.git"),
      "{}",
      redacted.text
    );
  }

  #[test]
  fn private_key_blocks_are_removed_wholesale() {
    let text = "before\n-----BEGIN RSA PRIVATE KEY-----\nQUJDREVG\nqk==\n-----END RSA PRIVATE KEY-----\nafter\n";
    let redacted = policy().apply(text);
    assert!(redacted.text.contains("before"), "{}", redacted.text);
    assert!(redacted.text.contains("after"), "{}", redacted.text);
    assert!(!redacted.text.contains("QUJDREVG"), "{}", redacted.text);
    assert_eq!(redacted.replacements, 1);
  }

  #[test]
  fn json_secret_fields_are_replaced_wholesale() {
    let mut value = json!({
      "api_key": "super-secret-value",
      "model": "plain-model-name",
      "nested": { "access_token": "tok", "note": "Bearer abcdefghijklmnop" },
      "base_url": "https://127.0.0.1:8080/v1",
    });
    let replacements = policy().apply_json(&mut value);
    assert!(replacements >= 3, "{replacements}");
    assert_eq!(value["api_key"], json!("[redacted:field]"));
    assert_eq!(value["nested"]["access_token"], json!("[redacted:field]"));
    assert!(
      !value.to_string().contains("super-secret-value"),
      "{}",
      value
    );
    assert_eq!(value["base_url"], json!("https://127.0.0.1:8080/v1"));
  }

  #[test]
  fn disabled_policy_is_explicit() {
    let off = RedactionPolicy {
      enabled: false,
      ..policy()
    };
    let redacted = off.apply("sk-abcdefghijklmnopqrstuvwxyz");
    assert_eq!(redacted.replacements, 0);
    assert!(redacted.text.contains("sk-abcdefghij"));
  }

  #[test]
  fn redaction_is_deterministic() {
    let text = "token project-token-abcdef123456 then sk-abcdefghijklmnopqrstuvwxyz";
    assert_eq!(policy().apply(text), policy().apply(text));
  }

  #[test]
  fn secret_name_heuristic_accepts_keys_and_rejects_endpoints() {
    assert!(looks_like_secret_name("OPENAI_API_KEY"));
    assert!(looks_like_secret_name("GH_TOKEN"));
    assert!(looks_like_secret_name("AWS_ACCESS_KEY_ID"));
    assert!(!looks_like_secret_name("PATH"));
    assert!(!looks_like_secret_name("RUST_LOG"));
    assert!(looks_like_endpoint("https://example.test"));
    assert!(looks_like_endpoint("/home/user/keys"));
    assert!(!looks_like_endpoint("hunteriesthisisasecretvalue"));
  }

  #[test]
  fn short_tokens_are_not_env_secret_candidates() {
    let policy = RedactionPolicy {
      min_secret_len: 64,
      ..policy()
    };
    assert!(
      policy.candidates().is_empty(),
      "no candidate may be shorter than the configured floor"
    );
  }
}
