//! Bridge from runtime events to the durable session and trace store.
//!
//! The store remains the sole sequence authority. The runtime supplies event
//! identity and semantic attribution; this adapter writes the event first, then
//! writes any session message against the sequence the store assigned.

use pi_rs_core::{
  AgentEvent, AttributedMessage, EventEnvelope, SessionCompactionRecord, SessionEpochRecord,
  SessionRecord, SinkError,
};
use pi_rs_store::Session;

use crate::turn::Trace;

/// A runtime trace backed by one durable store session.
#[derive(Debug)]
pub struct StoreTrace {
  session: Session,
  /// A compaction marker is only allowed to claim a persisted summary when the
  /// preceding runtime event opened one. This protects restoration from a
  /// hand-authored completion event with no summary message beside it.
  summary_pending: bool,
  /// Range opened by the most recent compaction epoch, carried into the compact
  /// session projection without introducing a second coordinate system.
  pending_compaction_range: Option<(pi_rs_core::EventSeq, pi_rs_core::EventSeq)>,
}

impl StoreTrace {
  pub fn new(session: Session) -> Self {
    Self {
      session,
      summary_pending: false,
      pending_compaction_range: None,
    }
  }

  pub fn session(&self) -> &Session {
    &self.session
  }

  pub fn into_session(self) -> Session {
    self.session
  }
}

impl Trace for StoreTrace {
  fn emit(&mut self, envelope: &mut EventEnvelope) -> Result<(), SinkError> {
    self.session.emit(envelope).map_err(store_error)?;
    // The trace is authoritative for high-resolution replay, while the session
    // log carries the small projection needed to resume without hydrating it.
    // Project transitions at the same boundary that assigns their canonical
    // sequence; the runtime never has to remember to write a second record.
    let record = match &envelope.event {
      AgentEvent::ContextSummary => {
        self.summary_pending = true;
        None
      }
      AgentEvent::ContextCompactionEpoch(epoch) => {
        self.pending_compaction_range = Some((epoch.replaces_from, epoch.replaces_through));
        None
      }
      AgentEvent::ModelEpochStarted(epoch) => Some(SessionRecord::Epoch(SessionEpochRecord {
        epoch: epoch.epoch,
        model: epoch.model.clone(),
        reason: epoch.reason.clone(),
      })),
      AgentEvent::ContextCompactionCompleted(completed) => {
        Some(SessionRecord::Compaction(SessionCompactionRecord {
          context_epoch: completed.context_epoch,
          level: completed.level,
          removed_messages: completed.removed_messages,
          // `retained_from` is a legacy session-line coordinate. The trace's
          // sequence range is authoritative; `retained_messages` lets resume
          // recover the model-visible tail without mixing coordinates.
          retained_from: 0,
          retained_messages: completed.retained_messages,
          summary_present: self.summary_pending
            && matches!(
              completed.level,
              pi_rs_core::ContextLevel::L1Ordinary | pi_rs_core::ContextLevel::L2Phase
            ),
          replaces_from: self.pending_compaction_range.map(|(from, _)| from),
          replaces_through: self.pending_compaction_range.map(|(_, through)| through),
        }))
      }
      _ => None,
    };
    if let Some(record) = record {
      self.session.record(&record).map_err(store_error)?;
    }
    if matches!(&envelope.event, AgentEvent::ContextCompactionCompleted(_)) {
      self.summary_pending = false;
      self.pending_compaction_range = None;
    }
    Ok(())
  }

  fn record_message(&mut self, attributed: &AttributedMessage) -> Result<(), SinkError> {
    let meta = &attributed.envelope.meta;
    let turn_id = meta
      .turn_id
      .as_ref()
      .ok_or_else(|| SinkError("a persisted message must belong to a turn".into()))?;
    let epoch = meta
      .model_epoch
      .ok_or_else(|| SinkError("a persisted message must carry a model epoch".into()))?;
    let model = meta
      .model
      .as_ref()
      .ok_or_else(|| SinkError("a persisted message must carry a model".into()))?;
    self
      .session
      .append_message(
        turn_id,
        &attributed.message,
        epoch,
        model,
        &attributed.envelope,
      )
      .map(|_| ())
      .map_err(store_error)
  }

  fn put_payload(&mut self, bytes: &[u8]) -> Result<Option<pi_rs_core::BlobRef>, SinkError> {
    self
      .session
      .put_recovery_blob(bytes)
      .map(Some)
      .map_err(store_error)
  }

  fn create_checkpoint(
    &mut self,
    capsule: &pi_rs_core::ContextCapsule,
  ) -> Result<Option<(pi_rs_core::CheckpointId, String)>, SinkError> {
    let record = self.session.checkpoint(capsule).map_err(store_error)?;
    Ok(Some((record.checkpoint_id, record.capsule_path)))
  }

  fn list_checkpoints(
    &self,
  ) -> Result<Vec<(pi_rs_core::CheckpointId, pi_rs_core::ContextCapsule)>, SinkError> {
    self.session.list_checkpoints().map_err(store_error)
  }

  fn flush(&mut self) -> Result<(), SinkError> {
    self.session.flush().map_err(store_error)
  }
}

fn store_error(error: pi_rs_store::StoreError) -> SinkError {
  SinkError(error.to_string())
}

#[cfg(test)]
mod tests {
  use pi_rs_core::{
    AgentEvent, AttributedMessage, EventMeta, ExternalContextRetrieved, ExternalContextSource,
    Message, ModelRef, SessionHeader, SessionId, TraceId, TurnId, UserMessage,
    session::SESSION_SCHEMA_VERSION,
  };
  use pi_rs_store::{StateLayout, Store, TempDir, TraceJournal, WritePolicy};

  use super::*;

  #[test]
  fn durable_messages_trace_and_recovery_blobs_share_redaction_policy() {
    let temp = TempDir::new("runtime-store-redaction");
    let secret = "configured-secret-91f7";
    let recognized = "sk-1234567890123456";
    let mut policy = WritePolicy::default();
    policy.redaction.scan_environment = false;
    policy.redaction.literals = vec![secret.into()];
    let store = Store::open(temp.path(), policy).unwrap();
    let session_id = SessionId::new();
    let model = ModelRef::new("local", "model");
    let session = store
      .begin(SessionHeader {
        session_id: session_id.clone(),
        version: SESSION_SCHEMA_VERSION,
        started_at_ms: 1,
        working_dir: "/workspace".into(),
        model: model.clone(),
        parent_session: None,
        branched_from_event: None,
        imported_from: None,
      })
      .unwrap();
    let turn_id = TurnId::new();
    let mut meta = EventMeta::new(session_id.clone(), TraceId::new());
    meta.turn_id = Some(turn_id);
    meta.model_epoch = Some(0);
    meta.model = Some(model);
    let text = format!("do not persist {secret} or {recognized}");
    let mut envelope = EventEnvelope::new(
      meta,
      AgentEvent::UserMessage(UserMessage {
        text: text.clone(),
        attachments: 0,
      }),
    );
    let mut trace = StoreTrace::new(session);
    trace.emit(&mut envelope).unwrap();
    trace
      .record_message(&AttributedMessage {
        envelope,
        message: Message::user(text),
      })
      .unwrap();
    let blob = trace
      .put_payload(format!("recovery {secret} or {recognized}").as_bytes())
      .unwrap()
      .unwrap();
    trace.flush().unwrap();

    let layout = StateLayout::new(temp.path());
    let session_bytes = std::fs::read(layout.session_path(&session_id)).unwrap();
    let trace_bytes = std::fs::read(layout.trace_path(&session_id)).unwrap();
    let blob_bytes = std::fs::read(layout.blob_path(&session_id, &blob)).unwrap();
    for bytes in [&session_bytes, &trace_bytes, &blob_bytes] {
      let durable = String::from_utf8_lossy(bytes);
      assert!(!durable.contains(secret), "secret persisted in {durable}");
      assert!(
        !durable.contains(recognized),
        "recognized key persisted in {durable}"
      );
      assert!(durable.contains("[redacted:"), "{durable}");
    }
  }

  #[test]
  fn external_context_reference_is_persisted_for_resume() {
    let temp = TempDir::new("runtime-store-external-context");
    let store = Store::open(temp.path(), WritePolicy::default()).unwrap();
    let session_id = SessionId::new();
    let model = ModelRef::new("local", "model");
    let session = store
      .begin(SessionHeader {
        session_id: session_id.clone(),
        version: SESSION_SCHEMA_VERSION,
        started_at_ms: 1,
        working_dir: "/workspace".into(),
        model: model.clone(),
        parent_session: None,
        branched_from_event: None,
        imported_from: None,
      })
      .unwrap();
    let turn_id = TurnId::new();
    let mut meta = EventMeta::new(session_id.clone(), TraceId::new());
    meta.turn_id = Some(turn_id.clone());
    meta.model_epoch = Some(0);
    meta.model = Some(model.clone());
    let source = ExternalContextSource {
      provider: "rkb-rs".into(),
      resource_id: "chunk-42".into(),
      provenance: "rkb-rs/agent-context".into(),
    };
    let mut metadata = std::collections::BTreeMap::new();
    metadata.insert("source_url".into(), "https://example.test/doc".into());
    let item = pi_rs_core::ExternalContextItem::inline(source, "evidence", Some("[1]".into()))
      .with_metadata(metadata.clone());
    let message = Message::user(item.format_for_model());
    let mut envelope = EventEnvelope::new(
      meta,
      AgentEvent::ExternalContextRetrieved(ExternalContextRetrieved {
        source: item.source.clone(),
        citation: item.citation.clone(),
        bytes: item.text.len() as u64,
        inline: true,
        metadata,
      }),
    );
    let mut trace = StoreTrace::new(session);
    trace.emit(&mut envelope).unwrap();
    trace
      .record_message(&AttributedMessage { envelope, message })
      .unwrap();
    trace.flush().unwrap();

    let restored = store.restore(&session_id).unwrap();
    let context = restored.messages[0]
      .external_context
      .as_ref()
      .expect("external context reference survives resume");
    assert_eq!(context.provider, "rkb-rs");
    assert_eq!(context.resource_id, "chunk-42");
    assert_eq!(context.citation.as_deref(), Some("[1]"));
    assert_eq!(context.metadata["source_url"], "https://example.test/doc");
    assert!(restored.messages[0].message.text().contains("evidence"));
  }

  #[test]
  fn store_assigns_the_event_sequence_used_by_the_session_message() {
    let temp = TempDir::new("runtime-store-trace");
    let store = Store::open(temp.path(), WritePolicy::default()).unwrap();
    let session_id = SessionId::new();
    let model = ModelRef::new("local", "model");
    let session = store
      .begin(SessionHeader {
        session_id: session_id.clone(),
        version: SESSION_SCHEMA_VERSION,
        started_at_ms: 1,
        working_dir: "/workspace".into(),
        model: model.clone(),
        parent_session: None,
        branched_from_event: None,
        imported_from: None,
      })
      .unwrap();
    let turn_id = TurnId::new();
    let mut meta = EventMeta::new(session_id.clone(), TraceId::new());
    meta.turn_id = Some(turn_id);
    meta.model_epoch = Some(0);
    meta.model = Some(model);
    let mut envelope = EventEnvelope::new(
      meta,
      AgentEvent::UserMessage(UserMessage {
        text: "hello".into(),
        attachments: 0,
      }),
    );
    let mut trace = StoreTrace::new(session);

    trace.emit(&mut envelope).unwrap();
    trace
      .record_message(&AttributedMessage {
        envelope: envelope.clone(),
        message: Message::user("hello"),
      })
      .unwrap();
    trace.flush().unwrap();

    let restored = store.restore(&session_id).unwrap();
    assert_eq!(restored.messages.len(), 1);
    assert_eq!(restored.messages[0].seq, envelope.meta.seq);
    assert_eq!(restored.messages[0].event_id, envelope.meta.event_id);
    let journal = TraceJournal::read(trace.session().trace_path()).unwrap();
    assert_eq!(journal.items.len(), 1);
    assert_eq!(journal.items[0].envelope.meta.seq, envelope.meta.seq);
    assert!(matches!(
      journal.items[0].envelope.event,
      AgentEvent::UserMessage(_)
    ));
    assert_eq!(
      journal.items[0].envelope.meta.model.as_ref(),
      Some(&restored.messages[0].model)
    );
    assert_eq!(
      journal.items[0].envelope.meta.model_epoch,
      Some(restored.messages[0].epoch)
    );
    assert_eq!(
      journal.items[0].envelope.event,
      AgentEvent::UserMessage(UserMessage {
        text: "hello".into(),
        attachments: 0
      })
    );
    assert_eq!(journal.items[0].envelope.meta.session_id, session_id);
    assert_eq!(restored.messages[0].message, Message::user("hello"));
  }
}
