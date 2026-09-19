//! Bridge from runtime events to the durable session and trace store.
//!
//! The store remains the sole sequence authority. The runtime supplies event
//! identity and semantic attribution; message-bearing boundaries use the store's
//! atomic event/projection transaction, while legacy already-sequenced callers
//! retain the compatibility completion path.

use pi_rs_core::{
  AgentEvent, AttributedMessage, EventEnvelope, Message, SessionCompactionRecord,
  SessionEpochRecord, SessionRecord, SessionReductionRecord, SinkError,
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
  /// Capsule written before the following `CheckpointCreated` event. The
  /// barrier is published only by that event's WAL transaction.
  pending_checkpoint: Option<pi_rs_core::SessionCheckpointRecord>,
  /// Compaction start is held in the WAL until its summary and completion are
  /// durable, so a crash cannot leave a projected summary with live history.
  pending_compaction_start: Option<pi_rs_core::EventId>,
}

impl StoreTrace {
  pub fn new(session: Session) -> Self {
    Self {
      session,
      summary_pending: false,
      pending_compaction_range: None,
      pending_checkpoint: None,
      pending_compaction_start: None,
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
    // The trace is authoritative for high-resolution replay, while the session
    // log carries the small projection needed to resume without hydrating it.
    // A WAL intent is prepared before either file is changed, so a crash cannot
    // silently leave one representation ahead of the other.
    let mut record = match &envelope.event {
      AgentEvent::ContextSummary => None,
      AgentEvent::ContextCompactionEpoch(epoch) => {
        self.pending_compaction_range = Some((epoch.replaces_from, epoch.replaces_through));
        None
      }
      AgentEvent::ModelEpochStarted(epoch) => Some(SessionRecord::Epoch(SessionEpochRecord {
        epoch: epoch.epoch,
        model: epoch.model.clone(),
        reason: epoch.reason.clone(),
      })),
      AgentEvent::CheckpointCreated(_) => self
        .pending_checkpoint
        .as_ref()
        .cloned()
        .map(SessionRecord::CheckpointBarrier),
      AgentEvent::ContextCompactionCompleted(completed) => {
        let has_summary = self.summary_pending
          && matches!(
            completed.level,
            pi_rs_core::ContextLevel::L1Ordinary | pi_rs_core::ContextLevel::L2Phase
          );
        let range = if has_summary {
          self.pending_compaction_range
        } else {
          None
        };
        Some(SessionRecord::Compaction(SessionCompactionRecord {
          context_epoch: completed.context_epoch,
          level: completed.level,
          removed_messages: completed.removed_messages,
          // `retained_from` is a legacy session-line coordinate. The trace's
          // sequence range is authoritative; `retained_messages` lets resume
          // recover the model-visible tail without mixing coordinates.
          retained_from: 0,
          retained_messages: completed.retained_messages,
          summary_present: has_summary,
          replaces_from: range.map(|(from, _)| from),
          replaces_through: range.map(|(_, through)| through),
          aborted: false,
          start_event_id: None,
          summary_event_id: None,
        }))
      }
      AgentEvent::ContextReduced(reduced) if reduced.removed_messages > 0 => {
        Some(SessionRecord::Reduction(SessionReductionRecord {
          event_id: envelope.meta.event_id.clone(),
          seq: None,
          reason: reduced.reason.clone(),
          removed_messages: reduced.removed_messages,
          retained_messages: reduced.retained_messages,
        }))
      }
      _ => None,
    };
    let hold_for_message = matches!(
      &envelope.event,
      AgentEvent::UserMessage(_)
        | AgentEvent::ExternalContextRetrieved(_)
        | AgentEvent::ContextSummary
        | AgentEvent::ToolCompleted(_)
        | AgentEvent::ToolFailed(_)
        | AgentEvent::ToolUnknown(_)
    ) || matches!(
      &envelope.event,
      AgentEvent::ModelRequestCompleted(completed) if completed.finish_reason.is_some()
    );
    let hold_for_compaction = matches!(&envelope.event, AgentEvent::ContextCompactionStarted(_));
    let transactional = record.is_some() || hold_for_message || hold_for_compaction;
    if transactional {
      self
        .session
        .emit_transaction(
          envelope,
          record.take(),
          hold_for_message || hold_for_compaction,
        )
        .map_err(store_error)?;
    } else {
      self.session.emit(envelope).map_err(store_error)?;
    }
    if hold_for_compaction {
      self.pending_compaction_start = Some(envelope.meta.event_id.clone());
    }
    if matches!(&envelope.event, AgentEvent::ContextCompactionCompleted(_)) {
      if let Some(start) = self.pending_compaction_start.take() {
        self
          .session
          .commit_projection_intent(&start)
          .map_err(store_error)?;
      }
    }
    if matches!(&envelope.event, AgentEvent::ContextSummary) {
      self.summary_pending = true;
    }
    if matches!(&envelope.event, AgentEvent::CheckpointCreated(_)) {
      self.pending_checkpoint = None;
    }
    if matches!(&envelope.event, AgentEvent::ContextCompactionCompleted(_)) {
      self.summary_pending = false;
      self.pending_compaction_range = None;
    }
    Ok(())
  }

  fn emit_message(
    &mut self,
    envelope: &mut EventEnvelope,
    message: &Message,
  ) -> Result<(), SinkError> {
    if envelope.meta.seq.is_some() {
      self
        .session
        .complete_message(&AttributedMessage {
          envelope: envelope.clone(),
          message: message.clone(),
        })
        .map(|_| ())
        .map_err(store_error)?;
    } else {
      self
        .session
        .emit_message(envelope, message)
        .map(|_| ())
        .map_err(store_error)?;
    }
    if matches!(&envelope.event, AgentEvent::ContextSummary) {
      self.summary_pending = true;
    }
    Ok(())
  }

  fn emit_without_message(&mut self, envelope: &mut EventEnvelope) -> Result<(), SinkError> {
    self.session.emit(envelope).map(|_| ()).map_err(store_error)
  }

  fn complete_without_message(
    &mut self,
    envelope: &pi_rs_core::EventEnvelope,
  ) -> Result<(), SinkError> {
    self
      .session
      .commit_projection_intent(&envelope.meta.event_id)
      .map_err(store_error)
  }

  fn record_message(&mut self, attributed: &AttributedMessage) -> Result<(), SinkError> {
    self
      .session
      .complete_message(attributed)
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
    let record = self
      .session
      .prepare_checkpoint(capsule)
      .map_err(store_error)?;
    self.pending_checkpoint = Some(record.clone());
    Ok(Some((record.checkpoint_id, record.capsule_path)))
  }

  fn set_checkpoint_context_epoch(&mut self, context_epoch: u32) -> Result<(), SinkError> {
    let Some(checkpoint) = self.pending_checkpoint.as_mut() else {
      return Err(SinkError(
        "checkpoint context epoch was set without a prepared checkpoint".into(),
      ));
    };
    checkpoint.context_epoch = context_epoch;
    Ok(())
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
    AgentEvent, EventMeta, ExternalContextRetrieved, ExternalContextSource, Message, ModelRef,
    SessionHeader, SessionId, ToolCallId, ToolFailed, ToolRequested, TraceId, TurnId, UserMessage,
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
    trace
      .emit_message(&mut envelope, &Message::user(&text))
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
    let restored = store.restore(&session_id).unwrap();
    assert_eq!(
      restored.messages[0].message.text(),
      "do not persist [redacted:field] or [redacted:key sk-]"
    );
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
    trace.emit_message(&mut envelope, &message).unwrap();
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

    trace
      .emit_message(&mut envelope, &Message::user("hello"))
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

  #[test]
  fn a_no_execution_tool_failure_closes_its_wal_intent() {
    let temp = pi_rs_store::TempDir::new("runtime-store-unexecuted-tool");
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
    let mut trace = StoreTrace::new(session);
    let mut request_meta = EventMeta::new(session_id.clone(), TraceId::new());
    request_meta.turn_id = Some(turn_id.clone());
    request_meta.model_epoch = Some(0);
    request_meta.model = Some(model.clone());
    let call_id = ToolCallId::new();
    let mut requested = EventEnvelope::new(
      request_meta,
      AgentEvent::ToolRequested(ToolRequested {
        call_id: call_id.clone(),
        name: "write".into(),
        arguments: serde_json::json!({"path":"a.txt", "content":"x"}),
        read_only: false,
      }),
    );
    trace.emit(&mut requested).unwrap();
    let mut failed_meta = EventMeta::new(session_id.clone(), TraceId::new());
    failed_meta.turn_id = Some(turn_id);
    failed_meta.model_epoch = Some(0);
    failed_meta.model = Some(model);
    let mut failed = EventEnvelope::new(
      failed_meta,
      AgentEvent::ToolFailed(ToolFailed {
        call_id,
        name: "write".into(),
        message: "tool was not executed".into(),
        duration_ms: 0,
        status: None,
      }),
    );
    trace.emit_without_message(&mut failed).unwrap();
    trace.into_session().finish().unwrap();

    let restored = store
      .restore(&session_id)
      .expect("explicit terminal closes the request");
    assert!(restored.interrupted_tools.is_empty());
  }
}
