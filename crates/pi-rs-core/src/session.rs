//! Semantic session state schema.
//!
//! Session state is what the runtime needs in order to continue: the ordered
//! messages, which model produced each of them, and pointers to checkpoints. It
//! is a *projection* of the canonical event stream, written so that resuming a
//! session does not require reading the high-resolution trace.
//!
//! Two shapes matter:
//!
//! - [`SessionRecord::CheckpointBarrier`] marks everything before it as
//!   summarized by a capsule. Resume cost is then `latest checkpoint + events
//!   after it`, which is what keeps large historical sessions cheap to open.
//! - [`SessionRecord::Message`] always carries model attribution, because a
//!   session may span several model epochs and "which model wrote this" must be
//!   answerable from session state alone.

use serde::{Deserialize, Serialize};

use crate::{
  capability::ModelRef,
  context::ContextCapsule,
  ids::{CheckpointId, EventId, EventSeq, SessionId, TurnId},
  message::{Message, Role},
};

/// Schema version stamped on the session header.
pub const SESSION_SCHEMA_VERSION: u32 = 1;

/// One line of `session.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum SessionRecord {
  /// First line of a session journal.
  Header(SessionHeader),
  /// A message that belongs to canonical history.
  Message(SessionMessage),
  /// A model epoch began at this point.
  Epoch(SessionEpochRecord),
  /// Context was compacted; older lines are still present but no longer part
  /// of the default model-visible set.
  Compaction(SessionCompactionRecord),
  /// A capsule summarizes everything up to this point.
  CheckpointBarrier(SessionCheckpointRecord),
}

/// Session metadata, written once and readable without parsing the rest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHeader {
  pub session_id: SessionId,
  pub version: u32,
  pub started_at_ms: u64,
  pub working_dir: String,
  /// Model that owned epoch 0. Later epochs are recorded per line.
  pub model: ModelRef,
  /// Session this one continues, when created by resume or branch.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub parent_session: Option<SessionId>,
  /// Event that created this session, when it was created by branching.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub branched_from_event: Option<EventId>,
  /// Provenance of a session imported from another tool, kept separate so an
  /// import never pretends to be native.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub imported_from: Option<String>,
}

/// A message with the attribution required for multi-epoch sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMessage {
  pub turn_id: TurnId,
  pub role: Role,
  pub message: Message,
  /// Epoch that produced this message. For user and tool messages this is the
  /// epoch that was active when they entered history.
  pub epoch: u32,
  pub model: ModelRef,
  /// The event that introduced the message, so a session line can always be
  /// traced back into the trace.
  pub event_id: EventId,
  /// Sequence number of that event, when the log had assigned one.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub seq: Option<EventSeq>,
}

/// A model epoch as recorded in session state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEpochRecord {
  pub epoch: u32,
  pub model: ModelRef,
  pub reason: crate::capability::EpochReason,
}

/// A compaction marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCompactionRecord {
  pub context_epoch: u32,
  pub level: crate::context::ContextLevel,
  pub removed_messages: u32,
  /// Line index (0-based, header excluded) of the first retained message.
  pub retained_from: u32,
}

/// A checkpoint barrier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCheckpointRecord {
  pub checkpoint_id: CheckpointId,
  pub capsule_version: u32,
  /// Relative path of the capsule file inside the session directory.
  pub capsule_path: String,
  /// Full capsule, duplicated here so that resume needs one read. Resume must
  /// not need the trace to know what the objective and constraints were.
  pub capsule: ContextCapsule,
}

/// Cheap session listing entry.
///
/// Built from headers plus a tail read, never from full hydration: session
/// metadata lookup is a startup-path concern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
  pub session_id: SessionId,
  pub started_at_ms: u64,
  pub working_dir: String,
  pub model: ModelRef,
  pub messages: u32,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub last_model: Option<ModelRef>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub last_turn_preview: Option<String>,
  pub closed: bool,
}

impl SessionSummary {
  pub fn from_header(header: &SessionHeader) -> Self {
    Self {
      session_id: header.session_id.clone(),
      started_at_ms: header.started_at_ms,
      working_dir: header.working_dir.clone(),
      model: header.model.clone(),
      messages: 0,
      last_model: None,
      last_turn_preview: None,
      closed: false,
    }
  }
}

#[cfg(test)]
mod tests {
  use crate::context::{CAPSULE_SCHEMA_VERSION, CapsuleArtifact, CapsuleDecision};

  use super::*;

  fn model() -> ModelRef {
    ModelRef::new("local", "qwen")
  }

  #[test]
  fn header_is_first_and_typed() {
    let header = SessionRecord::Header(SessionHeader {
      session_id: SessionId::new(),
      version: SESSION_SCHEMA_VERSION,
      started_at_ms: 1_700_000_000_000,
      working_dir: "/repo".into(),
      model: model(),
      parent_session: None,
      branched_from_event: None,
      imported_from: None,
    });
    let line = serde_json::to_string(&header).unwrap();
    assert!(line.contains("\"type\":\"header\""), "{line}");
    assert!(line.contains("\"version\":1"), "{line}");
    assert_eq!(
      serde_json::from_str::<SessionRecord>(&line).unwrap(),
      header
    );
  }

  #[test]
  fn messages_keep_model_attribution() {
    let record = SessionRecord::Message(SessionMessage {
      turn_id: TurnId::new(),
      role: Role::Assistant,
      message: Message::assistant("patch applied"),
      epoch: 2,
      model: ModelRef::new("backup", "small"),
      event_id: EventId::new(),
      seq: Some(EventSeq(41)),
    });
    let line = serde_json::to_string(&record).unwrap();
    assert!(line.contains("\"epoch\":2"), "{line}");
    assert!(line.contains("backup/small"), "{line}");
    assert_eq!(
      serde_json::from_str::<SessionRecord>(&line).unwrap(),
      record
    );
  }

  #[test]
  fn checkpoint_barrier_carries_the_capsule() {
    let capsule = ContextCapsule {
      version: CAPSULE_SCHEMA_VERSION,
      objective: "recover resume path".into(),
      completed_work: Vec::new(),
      decisions: vec![CapsuleDecision {
        decision: "duplicate capsule into session line".into(),
        rationale: "resume needs one read".into(),
      }],
      constraints: vec!["no trace hydration at startup".into()],
      current_state: "writing schema".into(),
      artifacts: vec![CapsuleArtifact {
        path: "crates/pi-rs-core/src/session.rs".into(),
        note: "schema".into(),
      }],
      unresolved: Vec::new(),
      next_actions: vec!["store implementation".into()],
    };
    let record = SessionRecord::CheckpointBarrier(SessionCheckpointRecord {
      checkpoint_id: CheckpointId::new(),
      capsule_version: CAPSULE_SCHEMA_VERSION,
      capsule_path: "checkpoints/0001.json".into(),
      capsule: capsule.clone(),
    });
    let line = serde_json::to_string(&record).unwrap();
    let decoded: SessionRecord = serde_json::from_str(&line).unwrap();
    let SessionRecord::CheckpointBarrier(barrier) = decoded else {
      panic!("expected checkpoint barrier");
    };
    assert_eq!(barrier.capsule, capsule);
    assert_eq!(barrier.capsule.constraints, capsule.constraints);
  }

  #[test]
  fn summary_needs_only_a_header() {
    let header = SessionHeader {
      session_id: SessionId::new(),
      version: SESSION_SCHEMA_VERSION,
      started_at_ms: 1,
      working_dir: "/repo".into(),
      model: model(),
      parent_session: None,
      branched_from_event: None,
      imported_from: Some("pi".into()),
    };
    let summary = SessionSummary::from_header(&header);
    assert_eq!(summary.session_id, header.session_id);
    assert_eq!(summary.messages, 0);
    assert!(!summary.closed);
    assert!(!serde_json::to_string(&summary).unwrap().contains("null"));
  }
}
