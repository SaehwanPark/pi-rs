//! Typed storage errors.
//!
//! Storage failures are operationally distinct: a failed append is usually
//! fatal for the current turn, a short or corrupt line is survivable, and a
//! missing session is an expected answer to a lookup. Collapsing all three into
//! `io::Error` would force callers to guess.

use std::fmt;

/// Storage failure.
#[derive(Debug)]
pub enum StoreError {
  /// The filesystem rejected an operation.
  Io(std::io::Error),
  /// A line could not be encoded.
  Encode(serde_json::Error),
  /// A line could not be decoded. The path and line number identify it so that
  /// a corrupt journal is diagnosable rather than merely fatal.
  Decode {
    path: String,
    line: usize,
    message: String,
  },
  /// The requested session or artifact does not exist.
  Missing(String),
  /// The on-disk shape contradicts the schema in a way that cannot be skipped
  /// safely, for example a session file with no header.
  Invalid(String),
}

impl StoreError {
  /// `true` when the failure is a missing file rather than damaged data.
  pub fn is_missing(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound
  }
}

impl fmt::Display for StoreError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Io(error) => write!(f, "storage i/o: {error}"),
      Self::Encode(error) => write!(f, "encode: {error}"),
      Self::Decode {
        path,
        line,
        message,
      } => {
        write!(f, "{path}:{line}: cannot decode line: {message}")
      }
      Self::Missing(what) => write!(f, "not found: {what}"),
      Self::Invalid(reason) => write!(f, "invalid state: {reason}"),
    }
  }
}

impl std::error::Error for StoreError {}

impl From<std::io::Error> for StoreError {
  fn from(value: std::io::Error) -> Self {
    Self::Io(value)
  }
}

impl From<serde_json::Error> for StoreError {
  fn from(value: serde_json::Error) -> Self {
    Self::Encode(value)
  }
}

impl From<std::fmt::Error> for StoreError {
  fn from(value: std::fmt::Error) -> Self {
    Self::Invalid(value.to_string())
  }
}
