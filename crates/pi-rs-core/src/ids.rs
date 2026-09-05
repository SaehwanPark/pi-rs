//! Durable identity types.
//!
//! Identity is generated once and then persisted. Nothing in the runtime may
//! re-derive an identifier from content, timestamps of nearby events, or
//! provider response fields, because identifiers are the join key between
//! session state, trace, tool lifecycle, replay, and failover continuity.
//!
//! Identifiers are UUIDv7-shaped (`8-4-4-4-12` hex, version 7) so that they:
//!
//! - sort by creation time, which keeps "latest session" listing cheap;
//! - look like the identifier shape used by the wider Pi ecosystem, which
//!   keeps session import/export honest;
//! - stay opaque to providers and to the model.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Wall-clock milliseconds since the Unix epoch.
///
/// Used both for event timestamps and for the time-ordered prefix of new
/// identifiers. No external clock dependency is introduced: this is the
/// platform clock, and correctness never depends on it being monotonic.
pub fn now_millis() -> u64 {
  std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap_or_default()
    .as_millis() as u64
}

fn random_bytes() -> [u8; 16] {
  let mut bytes = [0u8; 16];
  if getrandom::fill(&mut bytes).is_err() {
    // Without OS randomness a fallback must still avoid collisions inside one
    // process. A duplicated identifier across processes is possible but only
    // degrades listing order; it never merges two distinct sessions.
    let nanos = std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap_or_default()
      .as_nanos();
    let seed = (nanos as u64) ^ (std::process::id() as u64) ^ (bytes.len() as u64);
    for (i, chunk) in bytes.chunks_mut(8).enumerate() {
      let value = seed
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(i as u64)
        .to_le_bytes();
      chunk.copy_from_slice(&value[..chunk.len()]);
    }
  }
  bytes
}

/// Format a UUIDv7 string from the current time and random suffix.
pub fn uuidv7() -> String {
  let mut bytes = random_bytes();
  let time = now_millis().to_be_bytes();
  // RFC 9562 puts the 48-bit millisecond timestamp in the first six bytes. That
  // is the low 48 bits of the value: copying the high bytes instead yields an
  // identifier that still sorts by time, but whose prefix decodes to a date in
  // 1970, which is exactly the kind of plausible-but-wrong encoding that makes
  // an age-based policy delete the wrong sessions.
  bytes[0..6].copy_from_slice(&time[2..8]);
  bytes[6] = (bytes[6] & 0x0f) | 0x70; // version 7
  bytes[8] = (bytes[8] & 0x3f) | 0x80; // RFC 9562 variant
  let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
  format!(
    "{}-{}-{}-{}-{}",
    &hex[0..8],
    &hex[8..12],
    &hex[12..16],
    &hex[16..20],
    &hex[20..32]
  )
}

/// A per-session monotonic sequence number attached by the durable event log.
///
/// `event_id` identifies an event; `seq` is the authoritative ordering key for
/// one session's journal. Ordering claims always use `seq`, never timestamps,
/// because clocks may step backwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventSeq(pub u64);

macro_rules! identity_type {
  ($name:ident, $doc:literal) => {
    #[doc = $doc]
    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
    #[serde(transparent)]
    pub struct $name(String);

    impl $name {
      /// Mint a new time-ordered identifier.
      pub fn new() -> Self {
        Self(uuidv7())
      }

      /// Adopt an identifier that already exists on disk or came from Pi.
      pub fn from_string(value: impl Into<String>) -> Self {
        Self(value.into())
      }

      pub fn as_str(&self) -> &str {
        &self.0
      }

      pub fn into_string(self) -> String {
        self.0
      }
    }

    impl Default for $name {
      fn default() -> Self {
        Self::new()
      }
    }

    impl fmt::Display for $name {
      fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
      }
    }
  };
}

// The rule "avoid macros when ordinary code is clearer" is applied here on
// volume grounds: eight byte-identical 30-line newtypes would hide the fact
// that identity semantics are shared. The macro body is small, local, and
// fully expanded in the docs above.
identity_type!(SessionId, "Stable identifier for one session.");
identity_type!(TurnId, "Stable identifier for one user-initiated turn.");
identity_type!(
  ToolCallId,
  "Stable identifier for one tool invocation. The same id must survive request, execution, persistence, and any replay decision."
);
identity_type!(
  EventId,
  "Stable identifier for one event in one session journal."
);
identity_type!(
  TraceId,
  "Identifier grouping one causal execution, used for correlation and optional export."
);
identity_type!(
  SpanId,
  "Identifier of one causal node inside a trace, used to relate model requests to tool work."
);
identity_type!(
  CheckpointId,
  "Stable identifier for one episode checkpoint capsule."
);

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn uuidv7_has_shape_and_time_order() {
    let first = uuidv7();
    assert_eq!(first.len(), 36, "canonical form is 36 characters");
    assert_eq!(&first[14..15], "7", "version nibble is 7");
    let variant = &first[19..20];
    assert!("89ab".contains(variant), "variant nibble: {variant}");

    let second = uuidv7();
    assert_ne!(first, second);
    // The prefix must decode to real wall-clock time, not merely sort by it.
    let encoded = u64::from_str_radix(
      &first
        .chars()
        .filter(|c| *c != '-')
        .take(12)
        .collect::<String>(),
      16,
    )
    .unwrap();
    let now = now_millis();
    assert!(
      now.saturating_sub(encoded) < 5_000 && encoded.saturating_sub(now) < 5_000,
      "prefix {encoded} should be the current millisecond {now}"
    );
    // Ordering is guaranteed at the resolution of the 48-bit time prefix, not
    // between two identifiers minted inside one millisecond; the random suffix
    // decides that tie. Event ordering therefore uses EventSeq, and this
    // ordering is only a convenience for "newest first" listings.
    assert_eq!(
      &first[..14],
      &second[..14],
      "same millisecond shares a prefix"
    );
    std::thread::sleep(std::time::Duration::from_millis(2));
    let later = uuidv7();
    assert!(
      first < later,
      "later identifiers sort later: {first} {later}"
    );
  }

  #[test]
  fn identity_round_trips_through_json_transparently() {
    let id = SessionId::new();
    let encoded = serde_json::to_string(&id).unwrap();
    assert_eq!(encoded, format!("\"{id}\""));
    let decoded: SessionId = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, id);
  }
}
