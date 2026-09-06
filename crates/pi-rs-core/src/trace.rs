//! Content-addressed blob references and trace journal schema.
//!
//! The trace is high-resolution history. Large payloads must not be inlined
//! into every journal line, so payloads above a threshold are written once to
//! content-addressed storage and referenced by [`BlobRef`]. Content addressing
//! gives the trace three properties for free: identical payloads are stored
//! once, a reference can be verified against the bytes it points at, and a
//! reference remains meaningful after the surrounding event is reduced.
//!
//! Raw provider payload capture is opt-in. When it is disabled the runtime
//! still keeps normalized events; it simply never persists the provider's own
//! wire format.

use serde::{Deserialize, Serialize};

use crate::{event::EventEnvelope, hash::sha256_hex};

/// Schema version stamped onto trace records that need their own version.
pub const TRACE_SCHEMA_VERSION: u32 = 1;

/// Reference to one stored payload.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BlobRef {
  /// Lowercase hex SHA-256 of the stored bytes.
  pub hash: String,
  /// Byte length of the stored bytes.
  pub size: u64,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub content_type: Option<String>,
}

impl BlobRef {
  /// Derive the reference the store must use for these bytes.
  pub fn for_bytes(bytes: &[u8], content_type: Option<&str>) -> Self {
    Self {
      hash: sha256_hex(bytes),
      size: bytes.len() as u64,
      content_type: content_type.map(str::to_string),
    }
  }

  /// Truncated hash for display and for `recovery_ref` strings.
  pub fn short_hash(&self) -> String {
    self.hash.chars().take(12).collect()
  }

  /// Relative path used inside a session directory.
  ///
  /// Two levels of prefix sharding keep directories usable when a long session
  /// stores many payloads.
  pub fn relative_path(&self) -> String {
    format!(
      "blobs/{}/{}",
      &self.hash[..2.min(self.hash.len())],
      self.hash
    )
  }

  /// Recovery pointer a human or later model stage can act on alone.
  pub fn recovery_ref(&self) -> String {
    format!("{}:{}", self.relative_path(), self.short_hash())
  }
}

/// Whether raw provider wire payloads may be persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RawPayloadCapture {
  /// Default. Only normalized events are persisted.
  #[default]
  Disabled,
  /// Persist raw provider payloads, which may contain secrets or sensitive
  /// project content, for diagnosis.
  Enabled,
}

impl RawPayloadCapture {
  pub fn is_enabled(self) -> bool {
    matches!(self, Self::Enabled)
  }
}

/// Retention policy for one session's trace and blobs.
///
/// Defaults are intentionally bounded: an unbounded journal is a disk incident
/// waiting to happen, and trace data may contain secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceRetention {
  /// Delete whole older-than-this sessions' traces. `None` means no age limit.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub max_age_days: Option<u64>,
  /// Soft cap on total trace bytes under the state directory.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub max_bytes: Option<u64>,
  /// Payloads at or above this size go to blob storage instead of inline.
  pub inline_threshold_bytes: u64,
  pub raw_payload: RawPayloadCapture,
}

impl Default for TraceRetention {
  fn default() -> Self {
    Self {
      max_age_days: None,
      max_bytes: Some(512 * 1024 * 1024),
      inline_threshold_bytes: 8 * 1024,
      raw_payload: RawPayloadCapture::Disabled,
    }
  }
}

/// Provenance of externally sourced context.
///
/// Kept as a typed value rather than a string so that citation and durable
/// resource identity survive compaction: an inline excerpt may be reduced to a
/// reference, but the reference must still name the resource it came from.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExternalContextSource {
  pub provider: String,
  pub resource_id: String,
  /// Where the claim came from, for example `rkb-rs/citation` or `web`.
  pub provenance: String,
}

impl std::fmt::Display for ExternalContextSource {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(
      f,
      "{}:{}:{}",
      self.provenance, self.provider, self.resource_id
    )
  }
}

/// One event field whose bytes were stored out of the line.
///
/// A line that carries a whole tool output makes every reader of the journal pay
/// for it: `grep`, a tail, a resume that only needs the tail. The bytes are not
/// gone; they are stored once, content-addressed, and the field in the line keeps
/// a bounded preview that says how much it left out and where the rest is. This
/// record is the machine-readable form of the same claim, so a caller does not
/// have to parse prose out of a preview to learn the sizes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalizedField {
  /// Path of the field within the line, `/`-separated, for example `output` or
  /// `arguments/contents`. The event is flattened into the line, so a
  /// single-segment path names an event field; the envelope's own fields (`v`,
  /// `meta`, `type`, `redactions`, `raw_payload`, `raw_ref`, `externalized`) are
  /// never spilled, so the two cannot be confused.
  pub field: String,
  /// Durable blob reference in the form `blobs/<shard>/<hash>`, i.e. the same
  /// form a retention pass extracts liveness from. Written in full, not
  /// abbreviated, so following the pointer needs no config.
  pub reference: String,
  /// Bytes of the redacted value stored in the blob.
  pub bytes: u64,
  /// Bytes left in the line, including the marker.
  pub inline: u64,
}

/// One trace journal line.
///
/// The envelope is flattened so that a trace line stays readable and greppable
/// while carrying the trace-only bookkeeping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceEntry {
  #[serde(flatten)]
  pub envelope: EventEnvelope,
  /// How many redactions were applied before persisting this line. Recorded so
  /// that "was this sanitized?" is answerable from the file, not from config
  /// drift.
  #[serde(default, skip_serializing_if = "is_zero")]
  pub redactions: u32,
  /// `true` when a raw provider payload was attached to this line, which is
  /// only possible when raw capture is enabled.
  #[serde(default, skip_serializing_if = "is_false")]
  pub raw_payload: bool,
  /// Session-relative recovery pointer to the stored raw payload, as produced
  /// by [`BlobRef::recovery_ref`]. Kept separate from the event itself so that
  /// reducing the event never destroys the pointer back to the bytes.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub raw_ref: Option<String>,
  /// Fields spilled to blob storage so that this line stayed within the inline
  /// budget. Empty, and omitted, for the ordinary case.
  ///
  /// This is a separate field rather than a reuse of [`Self::raw_ref`]: a raw
  /// provider payload and a reduced normalized event are different claims about
  /// where the bytes came from, and one line can legitimately carry both.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub externalized: Vec<ExternalizedField>,
}

fn is_zero(value: &u32) -> bool {
  *value == 0
}

fn is_false(value: &bool) -> bool {
  !*value
}

#[cfg(test)]
mod tests {
  use crate::{
    event::{AgentEvent, Diagnostic, DiagnosticLevel},
    ids::{SessionId, TraceId},
  };

  use super::*;

  fn entry() -> TraceEntry {
    TraceEntry {
      envelope: EventEnvelope::new(
        crate::event::EventMeta::new(SessionId::new(), TraceId::new()),
        AgentEvent::Diagnostic(Diagnostic {
          level: DiagnosticLevel::Info,
          message: "hello".into(),
        }),
      ),
      redactions: 0,
      raw_payload: false,
      raw_ref: None,
      externalized: Vec::new(),
    }
  }

  #[test]
  fn blob_reference_is_content_derived_and_sharded() {
    let first = BlobRef::for_bytes(b"same payload", Some("text/plain"));
    let second = BlobRef::for_bytes(b"same payload", Some("text/plain"));
    let other = BlobRef::for_bytes(b"other payload", None);
    assert_eq!(first, second, "identical bytes must share one blob");
    assert_ne!(first, other);
    assert_eq!(first.relative_path().split('/').count(), 3);
    assert!(first.recovery_ref().starts_with("blobs/"));
    assert_eq!(first.size, 12);
  }

  #[test]
  fn raw_capture_is_opt_in() {
    assert_eq!(RawPayloadCapture::default(), RawPayloadCapture::Disabled);
    assert!(!RawPayloadCapture::default().is_enabled());
    assert!(TraceRetention::default().raw_payload == RawPayloadCapture::Disabled);
  }

  #[test]
  fn trace_line_is_flat_and_round_trips() {
    let base = entry();
    let line = serde_json::to_string(&base).unwrap();
    assert!(line.contains("\"type\":\"diagnostic\""), "{line}");
    assert!(
      !line.contains("redactions"),
      "quiet fields stay out: {line}"
    );
    let decoded: TraceEntry = serde_json::from_str(&line).unwrap();
    assert_eq!(decoded, base);

    let mut sensitive = entry();
    sensitive.redactions = 2;
    sensitive.raw_payload = true;
    sensitive.raw_ref = Some("blobs/ab/abcd:abcdef012345".into());
    let line = serde_json::to_string(&sensitive).unwrap();
    assert!(line.contains("\"redactions\":2"), "{line}");
    let decoded: TraceEntry = serde_json::from_str(&line).unwrap();
    assert_eq!(decoded, sensitive);
  }
}
