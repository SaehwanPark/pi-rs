//! Crash-recovery intents for the semantic session projection.
//!
//! Trace and session records live in separate append-only files, so a process
//! can die after one durable write and before the other. This small WAL makes
//! that window explicit: prepare before the canonical event, attach the
//! projection after the event receives its sequence, and commit only after the
//! semantic record is durable. Resume consumes any uncommitted intent before it
//! can contact a provider.
//!
//! Prepare entries contain only the event identity and the small lifecycle
//! discriminator needed by recovery, plus a bounded exact-message payload for
//! message transactions. Large messages are represented by a verified blob
//! reference, so crash metadata never becomes an unbounded second copy of the
//! trace.

use std::{
  collections::{BTreeMap, BTreeSet},
  path::{Path, PathBuf},
};

use rupi_core::{
  AgentEvent, BlobRef, EventEnvelope, EventId, Message, RedactionPolicy, SessionRecord, TurnId,
};
use serde::{Deserialize, Deserializer, Serialize};

use crate::{
  StoreError,
  jsonl::{LineWriter, read_jsonl_with_limits, recover_append_tail},
};

/// Maximum serialized size of one WAL line.
///
/// Inline message payloads are capped before they reach the WAL; this bound also
/// covers lifecycle records and checkpoint capsules so a crash marker cannot
/// become an unbounded second durable payload.
pub(crate) const MAX_WAL_LINE_BYTES: usize = 256 * 1024;
/// Total bytes accepted from one projection WAL before recovery refuses it.
///
/// A clean WAL is compacted after each committed transaction on supported
/// filesystems, so this is intentionally much larger than the normal crash
/// window while still bounding restart work and allocation.
const MAX_WAL_BYTES: u64 = 8 * 1024 * 1024;
/// Number of decoded WAL entries retained while reconstructing pending work.
const MAX_WAL_ENTRIES: usize = 16 * 1024;

/// Reject a semantic projection before its canonical event is appended. Held
/// WAL intents cannot safely carry arbitrarily large checkpoint capsules or
/// other records; preflighting avoids a canonical event with an unrecoverable
/// projection transaction behind it.
pub(crate) fn validate_projection_size(
  tx_id: &EventId,
  record: &SessionRecord,
  redaction: &RedactionPolicy,
) -> Result<(), StoreError> {
  let mut value = serde_json::to_value(WalEntry::Projection {
    tx_id: tx_id.clone(),
    record: Box::new(record.clone()),
  })?;
  redaction.apply_json(&mut value);
  let bytes = serde_json::to_vec(&value)?.len();
  if bytes > MAX_WAL_LINE_BYTES {
    return Err(StoreError::Invalid(format!(
      "projection WAL line exceeds the {MAX_WAL_LINE_BYTES}-byte bound"
    )));
  }
  Ok(())
}

/// The bounded discriminator required to classify a pending transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub(crate) enum WalEventKind {
  ContextCompactionStarted,
  ModelRequestCompleted { terminal: bool },
  Other,
}

/// Exact model-visible message bytes retained for recovery of a pending
/// transaction. Small messages stay in the bounded WAL; larger ones are stored
/// in the session blob store and the WAL keeps only the verified reference.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub(crate) enum MessageRecovery {
  Inline { message: Box<Message> },
  Blob { blob: BlobRef },
}

/// Compact replacement for a full event envelope in a prepare entry.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct WalPrepare {
  pub(crate) event_id: EventId,
  pub(crate) turn_id: Option<TurnId>,
  pub(crate) model_epoch: Option<u32>,
  pub(crate) kind: WalEventKind,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub(crate) recovery: Option<MessageRecovery>,
}

impl WalPrepare {
  fn from_envelope(envelope: &EventEnvelope) -> Self {
    let kind = match &envelope.event {
      AgentEvent::ContextCompactionStarted(_) => WalEventKind::ContextCompactionStarted,
      AgentEvent::ModelRequestCompleted(completed) => WalEventKind::ModelRequestCompleted {
        terminal: completed.finish_reason.is_some(),
      },
      _ => WalEventKind::Other,
    };
    Self {
      event_id: envelope.meta.event_id.clone(),
      turn_id: envelope.meta.turn_id.clone(),
      model_epoch: envelope.meta.model_epoch,
      kind,
      recovery: None,
    }
  }
}

impl<'de> Deserialize<'de> for WalPrepare {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    let value = serde_json::Value::deserialize(deserializer)?;
    // WALs written before prepare bounding stored the complete EventEnvelope.
    // Decode those entries only long enough to derive the compact discriminator;
    // all newly written entries use the bounded shape below.
    if value.get("meta").is_some() && value.get("event").is_some() {
      let envelope =
        serde_json::from_value::<EventEnvelope>(value).map_err(serde::de::Error::custom)?;
      return Ok(Self::from_envelope(&envelope));
    }
    #[derive(Deserialize)]
    struct CompactWalPrepare {
      event_id: EventId,
      turn_id: Option<TurnId>,
      model_epoch: Option<u32>,
      kind: WalEventKind,
      #[serde(default)]
      recovery: Option<MessageRecovery>,
    }
    let compact =
      serde_json::from_value::<CompactWalPrepare>(value).map_err(serde::de::Error::custom)?;
    Ok(Self {
      event_id: compact.event_id,
      turn_id: compact.turn_id,
      model_epoch: compact.model_epoch,
      kind: compact.kind,
      recovery: compact.recovery,
    })
  }
}

/// One append-only WAL line.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "op")]
enum WalEntry {
  Prepare {
    tx_id: EventId,
    envelope: WalPrepare,
  },
  Recovery {
    tx_id: EventId,
    recovery: MessageRecovery,
  },
  Projection {
    tx_id: EventId,
    record: Box<SessionRecord>,
  },
  Commit {
    tx_id: EventId,
  },
}

/// An intent whose trace event was prepared but whose projection transaction is
/// not yet committed.
#[derive(Debug, Clone)]
pub(crate) struct PendingProjection {
  pub tx_id: EventId,
  pub envelope: WalPrepare,
  pub record: Option<SessionRecord>,
}

/// Durable intent writer for one session.
#[derive(Debug)]
pub(crate) struct ProjectionWal {
  path: PathBuf,
  writer: LineWriter,
  redaction: RedactionPolicy,
}

impl ProjectionWal {
  /// Inspect a WAL without creating or opening it for append.
  pub fn pending_at(path: &Path) -> Result<Vec<PendingProjection>, StoreError> {
    if !path.exists() {
      return Ok(Vec::new());
    }
    Self::read_pending(path)
  }

  pub fn open(path: &Path, redaction: RedactionPolicy) -> Result<Self, StoreError> {
    recover_append_tail(path)?;
    // A malformed WAL line is not treated like a cosmetic trace delta. It may
    // hide an intent that must be reconciled, so reopening fails closed.
    let _ = Self::read_pending(path)?;
    Ok(Self {
      path: path.to_path_buf(),
      writer: LineWriter::create(path)?,
      redaction,
    })
  }

  pub fn prepare(&mut self, envelope: &EventEnvelope) -> Result<EventId, StoreError> {
    self.prepare_with_recovery(envelope, None)
  }

  pub fn prepare_message(
    &mut self,
    envelope: &EventEnvelope,
    recovery: MessageRecovery,
  ) -> Result<EventId, StoreError> {
    self.prepare_with_recovery(envelope, Some(recovery))
  }

  fn prepare_with_recovery(
    &mut self,
    envelope: &EventEnvelope,
    recovery: Option<MessageRecovery>,
  ) -> Result<EventId, StoreError> {
    let tx_id = envelope.meta.event_id.clone();
    let mut prepared = WalPrepare::from_envelope(envelope);
    prepared.recovery = recovery;
    self.append(&WalEntry::Prepare {
      tx_id: tx_id.clone(),
      envelope: prepared,
    })?;
    Ok(tx_id)
  }

  pub fn set_recovery(
    &mut self,
    tx_id: &EventId,
    recovery: MessageRecovery,
  ) -> Result<(), StoreError> {
    let pending = self.pending()?;
    let Some(intent) = pending.iter().find(|intent| intent.tx_id == *tx_id) else {
      return Err(StoreError::Invalid(format!(
        "projection WAL has no pending transaction {tx_id}"
      )));
    };
    if intent.envelope.recovery.is_some() {
      return Err(StoreError::Invalid(format!(
        "projection WAL transaction {tx_id} already has a recovery payload"
      )));
    }
    self.append(&WalEntry::Recovery {
      tx_id: tx_id.clone(),
      recovery,
    })
  }

  pub fn set_projection(
    &mut self,
    tx_id: &EventId,
    record: &SessionRecord,
  ) -> Result<(), StoreError> {
    self.append(&WalEntry::Projection {
      tx_id: tx_id.clone(),
      record: Box::new(record.clone()),
    })
  }

  pub fn commit(&mut self, tx_id: &EventId) -> Result<(), StoreError> {
    self.append(&WalEntry::Commit {
      tx_id: tx_id.clone(),
    })?;
    // A clean WAL is an implementation detail, not durable history. Compacting
    // it after the commit keeps reopen cost bounded by incomplete work rather
    // than by the number of successful messages in a long session.
    if self.pending()?.is_empty() {
      self.compact()?;
    }
    Ok(())
  }

  fn compact(&mut self) -> Result<(), StoreError> {
    self.writer.clear()
  }

  pub fn pending(&self) -> Result<Vec<PendingProjection>, StoreError> {
    Self::read_pending(&self.path)
  }

  fn read_pending(path: &Path) -> Result<Vec<PendingProjection>, StoreError> {
    if path.exists() {
      let bytes = std::fs::metadata(path)?.len();
      if bytes > MAX_WAL_BYTES {
        return Err(StoreError::Invalid(format!(
          "projection WAL {} exceeds the {MAX_WAL_BYTES}-byte bound",
          path.display()
        )));
      }
    }
    let report = if path.exists() {
      read_jsonl_with_limits::<WalEntry>(path, MAX_WAL_LINE_BYTES, MAX_WAL_ENTRIES)?
    } else {
      crate::jsonl::ReadReport {
        items: Vec::new(),
        malformed: 0,
        first_malformed_line: None,
      }
    };
    if report.malformed > 0 {
      return Err(StoreError::Invalid(format!(
        "projection WAL {} contains {} malformed line(s)",
        path.display(),
        report.malformed
      )));
    }
    let mut pending = BTreeMap::<EventId, PendingProjection>::new();
    let mut seen_tx_ids = BTreeSet::new();
    for entry in report.items {
      match entry {
        WalEntry::Prepare { tx_id, envelope } => {
          if !seen_tx_ids.insert(tx_id.clone()) {
            return Err(StoreError::Invalid(format!(
              "projection WAL {} reuses transaction {}",
              path.display(),
              tx_id
            )));
          }
          if pending.contains_key(&tx_id) {
            return Err(StoreError::Invalid(format!(
              "projection WAL {} has a duplicate prepare for {}",
              path.display(),
              tx_id
            )));
          }
          if envelope.event_id != tx_id {
            return Err(StoreError::Invalid(format!(
              "projection WAL {} prepare {} does not match its event {}",
              path.display(),
              tx_id,
              envelope.event_id
            )));
          }
          pending.insert(
            tx_id.clone(),
            PendingProjection {
              tx_id,
              envelope,
              record: None,
            },
          );
        }
        WalEntry::Recovery { tx_id, recovery } => {
          if let Some(intent) = pending.get_mut(&tx_id) {
            if intent.envelope.recovery.replace(recovery).is_some() {
              return Err(StoreError::Invalid(format!(
                "projection WAL {} has a duplicate recovery payload for {}",
                path.display(),
                tx_id
              )));
            }
          } else {
            return Err(StoreError::Invalid(format!(
              "projection WAL {} has a recovery payload without a prepare for {}",
              path.display(),
              tx_id
            )));
          }
        }
        WalEntry::Projection { tx_id, record } => {
          if matches!(record.as_ref(), SessionRecord::Header(_)) {
            return Err(StoreError::Invalid(format!(
              "projection WAL {} contains a header projection",
              path.display()
            )));
          }
          if let Some(intent) = pending.get_mut(&tx_id) {
            if intent.record.replace(*record).is_some() {
              return Err(StoreError::Invalid(format!(
                "projection WAL {} has a duplicate projection for {}",
                path.display(),
                tx_id
              )));
            }
          } else {
            return Err(StoreError::Invalid(format!(
              "projection WAL {} has a projection without a prepare",
              path.display()
            )));
          }
        }
        WalEntry::Commit { tx_id } => {
          if pending.remove(&tx_id).is_none() {
            return Err(StoreError::Invalid(format!(
              "projection WAL {} has a commit without a prepare",
              path.display()
            )));
          }
        }
      }
    }
    Ok(pending.into_values().collect())
  }

  fn append(&mut self, entry: &WalEntry) -> Result<(), StoreError> {
    let mut value = serde_json::to_value(entry)?;
    self.redaction.apply_json(&mut value);
    let line = serde_json::to_string(&value)?;
    if line.len() > MAX_WAL_LINE_BYTES {
      return Err(StoreError::Invalid(format!(
        "projection WAL line exceeds the {}-byte bound",
        MAX_WAL_LINE_BYTES
      )));
    }
    self.writer.write_line(&line, true)
  }
}

#[cfg(test)]
mod tests {
  use rupi_core::{
    Diagnostic, DiagnosticLevel, EventEnvelope, EventId, EventMeta, Message, RedactionPolicy,
    SessionId, SpanId, TraceId, TurnId, event::AgentEvent,
  };

  use crate::TempDir;

  use super::*;

  fn diagnostic(session: &SessionId) -> EventEnvelope {
    EventEnvelope::new(
      EventMeta {
        event_id: EventId::new(),
        session_id: session.clone(),
        turn_id: Some(TurnId::new()),
        seq: None,
        timestamp_ms: 1,
        model_epoch: None,
        model: None,
        tool_call_id: None,
        parent_event_id: None,
        trace_id: TraceId::new(),
        span_id: SpanId::new(),
      },
      AgentEvent::Diagnostic(Diagnostic {
        level: DiagnosticLevel::Info,
        message: "wal compaction".into(),
      }),
    )
  }

  #[test]
  fn legacy_prepare_entries_without_message_recovery_remain_readable() {
    let tmp = TempDir::new("projection-wal-legacy");
    let path = tmp.child("session.wal.jsonl");
    let session = SessionId::from_string("018f-wal-legacy");
    let envelope = diagnostic(&session);
    let event_id = envelope.meta.event_id.clone();
    let line = serde_json::json!({
      "op": "prepare",
      "tx_id": event_id.clone(),
      "envelope": {
        "event_id": event_id,
        "turn_id": envelope.meta.turn_id,
        "model_epoch": envelope.meta.model_epoch,
        "kind": serde_json::to_value(WalEventKind::Other).unwrap()
      }
    });
    std::fs::write(
      &path,
      format!("{}\n", serde_json::to_string(&line).unwrap()),
    )
    .unwrap();
    let pending = ProjectionWal::pending_at(&path).unwrap();
    assert_eq!(pending.len(), 1);
    assert!(pending[0].envelope.recovery.is_none());
    assert_eq!(pending[0].tx_id, envelope.meta.event_id);
  }

  #[test]
  fn inline_message_recovery_is_redacted_and_stays_within_the_wal_bound() {
    let tmp = TempDir::new("projection-wal-message");
    let path = tmp.child("session.wal.jsonl");
    let session = SessionId::from_string("018f-wal-message");
    let secret = "projection-secret";
    let policy = RedactionPolicy {
      scan_environment: false,
      literals: vec![secret.into()],
      ..RedactionPolicy::default()
    };
    let mut wal = ProjectionWal::open(&path, policy).unwrap();
    let envelope = diagnostic(&session);
    wal
      .prepare_message(
        &envelope,
        MessageRecovery::Inline {
          message: Box::new(Message::user(secret)),
        },
      )
      .unwrap();
    let pending = wal.pending().unwrap();
    let Some(MessageRecovery::Inline { message }) = pending[0].envelope.recovery.as_ref() else {
      panic!("small messages stay inline")
    };
    assert_eq!(message.text(), "[redacted:field]");
    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert!(!on_disk.contains(secret));
    assert!(on_disk.len() < MAX_WAL_LINE_BYTES);
  }

  #[test]
  fn committed_wal_can_be_compacted_and_reused() {
    let tmp = TempDir::new("projection-wal-reuse");
    let path = tmp.child("session.wal.jsonl");
    let session = SessionId::from_string("018f-wal");
    let mut wal = ProjectionWal::open(&path, RedactionPolicy::default()).unwrap();

    let first = diagnostic(&session);
    wal.prepare(&first).unwrap();
    wal.commit(&first.meta.event_id).unwrap();
    assert!(wal.pending().unwrap().is_empty());
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);

    let second = diagnostic(&session);
    wal.prepare(&second).unwrap();
    wal.commit(&second.meta.event_id).unwrap();
    assert!(wal.pending().unwrap().is_empty());
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
  }
}
