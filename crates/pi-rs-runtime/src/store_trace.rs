//! Bridge from runtime events to the durable session and trace store.
//!
//! The store remains the sole sequence authority. The runtime supplies event
//! identity and semantic attribution; this adapter writes the event first, then
//! writes any session message against the sequence the store assigned.

use pi_rs_core::{AttributedMessage, EventEnvelope, SinkError};
use pi_rs_store::Session;

use crate::turn::Trace;

/// A runtime trace backed by one durable store session.
#[derive(Debug)]
pub struct StoreTrace {
  session: Session,
}

impl StoreTrace {
  pub fn new(session: Session) -> Self {
    Self { session }
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
    self.session.emit(envelope).map(|_| ()).map_err(store_error)
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
    AgentEvent, EventMeta, Message, ModelRef, SessionHeader, SessionId, TraceId, TurnId,
    UserMessage, session::SESSION_SCHEMA_VERSION,
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
