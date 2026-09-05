//! Semantic session log (`sessions/<id>.jsonl`).
//!
//! The session log is what the runtime needs in order to continue: the header,
//! the messages with model attribution, epoch records, compaction markers, and
//! checkpoint barriers. It is a projection of the canonical trace, written so
//! that resuming does not require reading the trace at all.
//!
//! Every line is written durably. Unlike trace deltas, a lost message is not a
//! cosmetic loss: it silently changes what the next model request believes
//! happened. Bounding resume cost is the checkpoint barrier's job, not the
//! writer's.

use std::path::Path;

use pi_rs_core::{
  capability::ModelRef,
  event::AgentEvent,
  ids::{EventSeq, SessionId},
  session::{SESSION_SCHEMA_VERSION, SessionHeader, SessionMessage, SessionRecord, SessionSummary},
};

use crate::{
  StoreError,
  jsonl::{LineWriter, ReadReport, read_first_line, read_jsonl},
};

/// Bytes read from a trace tail to decide whether a session is closed.
const TAIL_WINDOW: u64 = 32 * 1024;

/// Writer and reader for one session's semantic state.
#[derive(Debug)]
pub struct SessionLog {
  writer: LineWriter,
  header: SessionHeader,
  records: usize,
}

impl SessionLog {
  /// Create a session log and write its header.
  ///
  /// The header is written through immediately: a session that exists at all
  /// must be listable, even if the process dies before the first message.
  pub fn create(path: &Path, header: SessionHeader) -> Result<Self, StoreError> {
    if read_first_line(path)?.is_some() {
      return Err(StoreError::Invalid(format!(
        "session {} already exists; a session log is never truncated",
        path.display()
      )));
    }
    let header_line = serde_json::to_string(&SessionRecord::Header(header.clone()))?;
    let mut writer = LineWriter::create(path)?;
    writer.write_line(&header_line, true)?;
    Ok(Self {
      writer,
      header,
      records: 1,
    })
  }

  /// Reopen an existing session log for appending.
  pub fn resume(path: &Path) -> Result<Self, StoreError> {
    let header = Self::read_header(path)?;
    let records = read_jsonl::<SessionRecord>(path)?.items.len();
    Ok(Self {
      writer: LineWriter::create(path)?,
      header,
      records: records.max(1),
    })
  }

  pub fn path(&self) -> &Path {
    self.writer.path()
  }

  pub fn header(&self) -> &SessionHeader {
    &self.header
  }

  pub fn session_id(&self) -> &SessionId {
    &self.header.session_id
  }

  /// Records written so far, including the header.
  pub fn records(&self) -> usize {
    self.records
  }

  /// Append one record. Session records are always durable.
  pub fn append(&mut self, record: &SessionRecord) -> Result<(), StoreError> {
    if matches!(record, SessionRecord::Header(_)) {
      return Err(StoreError::Invalid(
        "a session header is written exactly once, at creation".into(),
      ));
    }
    let line = serde_json::to_string(record)?;
    self.writer.write_line(&line, true)?;
    self.records += 1;
    Ok(())
  }

  pub fn flush(&mut self) -> Result<(), StoreError> {
    self.writer.flush()
  }

  /// Read and validate only the header.
  ///
  /// This is the listing primitive: session metadata must not require reading
  /// message bodies.
  pub fn read_header(path: &Path) -> Result<SessionHeader, StoreError> {
    let line = read_first_line(path)?
      .ok_or_else(|| StoreError::Invalid(format!("{} has no session header", path.display())))?;
    let record: SessionRecord =
      serde_json::from_str(&line).map_err(|error| StoreError::Decode {
        path: path.display().to_string(),
        line: 1,
        message: error.to_string(),
      })?;
    match record {
      SessionRecord::Header(header) => {
        validate_header(&header, path)?;
        Ok(header)
      }
      other => Err(StoreError::Invalid(format!(
        "{} must begin with a session header, found {other:?}",
        path.display()
      ))),
    }
  }

  /// Read every record.
  pub fn read(path: &Path) -> Result<ReadReport<SessionRecord>, StoreError> {
    read_jsonl(path)
  }

  /// Cheap summary: header plus a bounded read of the trace tail.
  ///
  /// Message counting is intentionally left to [`SessionLog::summary_report`].
  /// Counting messages requires reading every message, which is exactly the
  /// cost that makes `list sessions` slow on long histories. The
  /// closed-or-not flag, by contrast, is available from the last trace line.
  pub fn summary(path: &Path, trace_path: &Path) -> Result<SessionSummary, StoreError> {
    let header = Self::read_header(path)?;
    let mut summary = SessionSummary::from_header(&header);
    summary.closed = trace_ends_the_session(trace_path).unwrap_or(false);
    Ok(summary)
  }

  /// Full summary for one session, including counts and a preview.
  pub fn summary_report(
    path: &Path,
    trace_path: &Path,
    preview_chars: usize,
  ) -> Result<SessionSummary, StoreError> {
    let header = Self::read_header(path)?;
    let mut summary = SessionSummary::from_header(&header);
    let report = Self::read(path)?;
    let mut messages = 0u32;
    let mut last_model: Option<ModelRef> = None;
    let mut preview: Option<String> = None;
    for record in &report.items {
      match record {
        SessionRecord::Message(message) => {
          messages += 1;
          if message.role == pi_rs_core::message::Role::User {
            preview = preview_of(&message.message.text(), preview_chars);
          }
        }
        SessionRecord::Epoch(epoch) => last_model = Some(epoch.model.clone()),
        _ => {}
      }
    }
    summary.messages = messages;
    summary.last_model = last_model.or(Some(header.model.clone()));
    summary.last_turn_preview = preview;
    summary.closed = trace_ends_the_session(trace_path).unwrap_or(false);
    Ok(summary)
  }

  /// Highest sequence number carried by session records.
  pub fn last_seq(path: &Path) -> Result<Option<EventSeq>, StoreError> {
    let mut last: Option<EventSeq> = None;
    for record in Self::read(path)?.items {
      if let SessionRecord::Message(message) = record {
        if let Some(seq) = message.seq {
          crate::journal::keep_max(&mut last, seq);
        }
      }
    }
    Ok(last)
  }
}

fn validate_header(header: &SessionHeader, path: &Path) -> Result<(), StoreError> {
  if header.version > SESSION_SCHEMA_VERSION {
    return Err(StoreError::Invalid(format!(
      "{} uses session schema version {}, newer than this build supports ({SESSION_SCHEMA_VERSION})",
      path.display(),
      header.version
    )));
  }
  crate::StateLayout::validate_session_id(&header.session_id)
}

fn trace_ends_the_session(trace_path: &Path) -> Result<bool, StoreError> {
  if !trace_path.exists() {
    return Ok(false);
  }
  let tail =
    crate::jsonl::read_jsonl_tail::<pi_rs_core::trace::TraceEntry>(trace_path, TAIL_WINDOW)?;
  Ok(
    tail
      .items
      .iter()
      .any(|entry| matches!(entry.envelope.event, AgentEvent::SessionEnded(_))),
  )
}

fn preview_of(text: &str, max_chars: usize) -> Option<String> {
  let trimmed = text.trim();
  if trimmed.is_empty() {
    return None;
  }
  let mut preview: String = trimmed.chars().take(max_chars).collect();
  if trimmed.chars().count() > max_chars {
    preview.push('…');
  }
  Some(preview)
}

/// Session state needed to continue, reconstructed from one file.
#[derive(Debug, Clone)]
pub struct RestoredSession {
  pub header: SessionHeader,
  /// Messages the next request should see. When a checkpoint barrier exists
  /// this is the post-barrier window only, which is what keeps resuming a long
  /// session cheap.
  pub messages: Vec<SessionMessage>,
  /// Capsule from the latest checkpoint barrier, if any.
  pub checkpoint: Option<pi_rs_core::context::ContextCapsule>,
  /// Sequence number of the barrier, so that post-checkpoint trace events can
  /// be read without a full scan.
  pub checkpoint_seq: Option<EventSeq>,
  pub epochs: Vec<pi_rs_core::session::SessionEpochRecord>,
  pub compactions: Vec<pi_rs_core::session::SessionCompactionRecord>,
  /// Messages summarized by the checkpoint, for honest UI reporting.
  pub summarized_messages: usize,
  /// Sequence of the last record this session log knows about.
  pub last_seq: Option<EventSeq>,
  pub malformed_records: usize,
  pub total_records: usize,
}

/// Restore session state as `latest checkpoint + records after it`.
pub fn restore(path: &Path) -> Result<RestoredSession, StoreError> {
  let header = SessionLog::read_header(path)?;
  let report = SessionLog::read(path)?;
  let mut messages: Vec<SessionMessage> = Vec::new();
  let mut epochs = Vec::new();
  let mut compactions = Vec::new();
  let mut checkpoint = None;
  let mut checkpoint_seq = None;
  let mut summarized_messages = 0usize;
  let mut last_seq: Option<EventSeq> = None;
  for record in report.items.iter().skip(1) {
    match record {
      SessionRecord::Message(message) => {
        if let Some(seq) = message.seq {
          crate::journal::keep_max(&mut last_seq, seq);
        }
        messages.push(message.clone());
      }
      SessionRecord::Epoch(epoch) => epochs.push(epoch.clone()),
      SessionRecord::Compaction(compaction) => compactions.push(compaction.clone()),
      SessionRecord::CheckpointBarrier(barrier) => {
        // A later barrier supersedes an earlier one: everything before it is
        // already inside the newer capsule's scope.
        summarized_messages += messages.len();
        checkpoint = Some(barrier.capsule.clone());
        checkpoint_seq = last_seq;
        messages.clear();
      }
      SessionRecord::Header(_) => {}
    }
  }
  Ok(RestoredSession {
    header,
    messages,
    checkpoint,
    checkpoint_seq,
    epochs,
    compactions,
    summarized_messages,
    last_seq,
    malformed_records: report.malformed,
    total_records: report.items.len(),
  })
}

#[cfg(test)]
mod tests {
  use pi_rs_core::{
    capability::EpochReason,
    context::{CAPSULE_SCHEMA_VERSION, ContextCapsule},
    ids::uuidv7,
    ids::{CheckpointId, EventId, TurnId},
    message::Message,
    session::{SessionCheckpointRecord, SessionEpochRecord, SessionMessage},
  };

  use crate::{StateLayout, tmp::TempDir};

  use super::*;

  fn header(session: &SessionId) -> SessionHeader {
    SessionHeader {
      session_id: session.clone(),
      version: SESSION_SCHEMA_VERSION,
      started_at_ms: 1_700_000_000_000,
      working_dir: "/repo".into(),
      model: ModelRef::new("local", "qwen"),
      parent_session: None,
      branched_from_event: None,
      imported_from: None,
    }
  }

  fn message(text: &str, seq: u64) -> SessionRecord {
    SessionRecord::Message(SessionMessage {
      turn_id: TurnId::new(),
      role: pi_rs_core::message::Role::User,
      message: Message::user(text),
      epoch: 0,
      model: ModelRef::new("local", "qwen"),
      event_id: EventId::new(),
      seq: Some(EventSeq(seq)),
    })
  }

  fn session(tmp: &TempDir) -> (StateLayout, SessionId) {
    let layout = StateLayout::new(tmp.path());
    layout.create().unwrap();
    (layout, SessionId::from_string(uuidv7()))
  }

  fn path(layout: &StateLayout, id: &SessionId) -> std::path::PathBuf {
    layout.session_path(id)
  }

  #[test]
  fn header_is_written_through_at_creation() {
    let tmp = TempDir::new("sessionlog-create");
    let (layout, id) = session(&tmp);
    let target = path(&layout, &id);
    {
      let log = SessionLog::create(&target, header(&id)).unwrap();
      assert_eq!(log.records(), 1);
      assert_eq!(log.session_id(), &id);
    }
    // No flush was requested: the session is already listable.
    assert_eq!(SessionLog::read_header(&target).unwrap().session_id, id);
  }

  #[test]
  fn a_second_header_is_refused_rather_than_appended() {
    let tmp = TempDir::new("sessionlog-double");
    let (layout, id) = session(&tmp);
    let target = path(&layout, &id);
    let mut log = SessionLog::create(&target, header(&id)).unwrap();
    let error = log.append(&SessionRecord::Header(header(&id))).unwrap_err();
    assert!(matches!(error, StoreError::Invalid(_)), "{error}");
    assert_eq!(log.records(), 1);
  }

  #[test]
  fn create_never_truncates_an_existing_session() {
    let tmp = TempDir::new("sessionlog-twice");
    let (layout, id) = session(&tmp);
    let target = path(&layout, &id);
    let _ = SessionLog::create(&target, header(&id)).unwrap();
    let error = SessionLog::create(&target, header(&id)).unwrap_err();
    assert!(matches!(error, StoreError::Invalid(_)), "{error}");
    assert_eq!(SessionLog::read(&target).unwrap().items.len(), 1);
  }

  #[test]
  fn resume_appends_after_the_existing_records() {
    let tmp = TempDir::new("sessionlog-resume");
    let (layout, id) = session(&tmp);
    let target = path(&layout, &id);
    {
      let mut log = SessionLog::create(&target, header(&id)).unwrap();
      log.append(&message("first", 1)).unwrap();
      log.append(&message("second", 2)).unwrap();
    }
    let mut reopened = SessionLog::resume(&target).unwrap();
    assert_eq!(reopened.records(), 3);
    reopened.append(&message("third", 3)).unwrap();
    drop(reopened);
    let report = SessionLog::read(&target).unwrap();
    assert_eq!(report.items.len(), 4);
    assert_eq!(SessionLog::last_seq(&target).unwrap(), Some(EventSeq(3)));
  }

  #[test]
  fn header_version_is_checked() {
    let tmp = TempDir::new("sessionlog-version");
    let (layout, id) = session(&tmp);
    let target = path(&layout, &id);
    let future = SessionRecord::Header(SessionHeader {
      version: SESSION_SCHEMA_VERSION + 1,
      ..header(&id)
    });
    std::fs::write(
      &target,
      format!("{}\n", serde_json::to_string(&future).unwrap()),
    )
    .unwrap();
    let error = SessionLog::read_header(&target).unwrap_err();
    assert!(matches!(error, StoreError::Invalid(_)), "{error}");
  }

  #[test]
  fn empty_or_headless_files_are_reported_as_invalid() {
    let tmp = TempDir::new("sessionlog-bad");
    let (layout, id) = session(&tmp);
    let target = path(&layout, &id);
    std::fs::write(&target, "").unwrap();
    assert!(matches!(
      SessionLog::read_header(&target),
      Err(StoreError::Invalid(_))
    ));
    std::fs::write(&target, "garbage\n").unwrap();
    assert!(matches!(
      SessionLog::read_header(&target),
      Err(StoreError::Decode { .. })
    ));
    let line = serde_json::to_string(&message("no header", 1)).unwrap();
    std::fs::write(&target, format!("{line}\n")).unwrap();
    let error = SessionLog::read_header(&target).unwrap_err();
    assert!(
      format!("{error}").contains("must begin with a session header"),
      "{error}"
    );
  }

  #[test]
  fn barrier_shortens_what_resume_needs() {
    let tmp = TempDir::new("sessionlog-barrier");
    let (layout, id) = session(&tmp);
    let target = path(&layout, &id);
    let mut log = SessionLog::create(&target, header(&id)).unwrap();
    for seq in 1..=5u64 {
      log.append(&message(&format!("old {seq}"), seq)).unwrap();
    }
    log
      .append(&SessionRecord::Epoch(SessionEpochRecord {
        epoch: 1,
        model: ModelRef::new("backup", "small"),
        reason: EpochReason::AutomaticFailover,
      }))
      .unwrap();
    log
      .append(&SessionRecord::CheckpointBarrier(SessionCheckpointRecord {
        checkpoint_id: CheckpointId::new(),
        capsule_version: CAPSULE_SCHEMA_VERSION,
        capsule_path: "checkpoints/cp.json".into(),
        capsule: ContextCapsule {
          version: CAPSULE_SCHEMA_VERSION,
          objective: "recover resume path".into(),
          completed_work: vec![],
          decisions: vec![],
          constraints: vec![],
          current_state: "resuming".into(),
          artifacts: vec![],
          unresolved: vec![],
          next_actions: vec![],
        },
      }))
      .unwrap();
    log.append(&message("after checkpoint", 6)).unwrap();
    drop(log);

    let restored = restore(&target).unwrap();
    assert_eq!(
      restored.total_records, 9,
      "header + 5 messages + epoch + barrier + message"
    );
    assert_eq!(restored.summarized_messages, 5);
    assert_eq!(restored.messages.len(), 1);
    assert_eq!(restored.messages[0].message.text(), "after checkpoint");
    assert_eq!(
      restored.checkpoint.as_ref().unwrap().objective,
      "recover resume path"
    );
    assert_eq!(restored.checkpoint_seq, Some(EventSeq(5)));
    assert_eq!(restored.last_seq, Some(EventSeq(6)));
    assert_eq!(restored.epochs.len(), 1);
  }

  #[test]
  fn a_second_barrier_supersedes_the_first() {
    let tmp = TempDir::new("sessionlog-two-barriers");
    let (layout, id) = session(&tmp);
    let target = path(&layout, &id);
    let mut log = SessionLog::create(&target, header(&id)).unwrap();
    log.append(&message("a", 1)).unwrap();
    let barrier = |path: &str| {
      SessionRecord::CheckpointBarrier(SessionCheckpointRecord {
        checkpoint_id: CheckpointId::new(),
        capsule_version: CAPSULE_SCHEMA_VERSION,
        capsule_path: path.into(),
        capsule: ContextCapsule {
          version: CAPSULE_SCHEMA_VERSION,
          objective: format!("capsule from {path}"),
          completed_work: vec![],
          decisions: vec![],
          constraints: vec![],
          current_state: "resuming".into(),
          artifacts: vec![],
          unresolved: vec![],
          next_actions: vec![],
        },
      })
    };
    log.append(&barrier("checkpoints/one.json")).unwrap();
    log.append(&message("b", 2)).unwrap();
    log.append(&barrier("checkpoints/two.json")).unwrap();
    log.append(&message("c", 3)).unwrap();
    drop(log);

    let restored = restore(&target).unwrap();
    assert_eq!(restored.messages.len(), 1);
    assert_eq!(restored.messages[0].message.text(), "c");
    assert_eq!(
      restored.checkpoint.as_ref().unwrap().objective,
      "capsule from checkpoints/two.json"
    );
    assert_eq!(
      restored.summarized_messages, 2,
      "both summarized windows count"
    );
  }

  #[test]
  fn summaries_are_cheap_and_detailed_on_request() {
    let tmp = TempDir::new("sessionlog-summary");
    let (layout, id) = session(&tmp);
    let target = path(&layout, &id);
    let trace = layout.trace_path(&id);
    let mut log = SessionLog::create(&target, header(&id)).unwrap();
    log.append(&message("long task description that should be previewed and clipped because it exceeds the configured preview length entirely", 1))
      .unwrap();
    log
      .append(&SessionRecord::Epoch(SessionEpochRecord {
        epoch: 1,
        model: ModelRef::new("backup", "small"),
        reason: EpochReason::AutomaticFailover,
      }))
      .unwrap();
    drop(log);

    let cheap = SessionLog::summary(&target, &trace).unwrap();
    assert_eq!(cheap.session_id, id);
    assert_eq!(cheap.messages, 0, "cheap summary does not count messages");
    assert!(!cheap.closed);
    assert_eq!(cheap.model, ModelRef::new("local", "qwen"));

    let detailed = SessionLog::summary_report(&target, &trace, 24).unwrap();
    assert_eq!(detailed.messages, 1);
    assert_eq!(detailed.last_model, Some(ModelRef::new("backup", "small")));
    let preview = detailed.last_turn_preview.unwrap();
    assert!(preview.chars().count() <= 25, "{preview}");
    assert!(preview.ends_with('…'));
  }

  #[test]
  fn restore_reports_malformed_records() {
    let tmp = TempDir::new("sessionlog-malformed");
    let (layout, id) = session(&tmp);
    let target = path(&layout, &id);
    {
      let mut log = SessionLog::create(&target, header(&id)).unwrap();
      log.append(&message("kept", 1)).unwrap();
    }
    std::fs::OpenOptions::new()
      .append(true)
      .open(&target)
      .and_then(|mut file| {
        use std::io::Write;
        file.write_all(b"{\"type\":\"message\",\"cut\n")
      })
      .unwrap();
    let restored = restore(&target).unwrap();
    assert_eq!(restored.messages.len(), 1);
    assert_eq!(restored.malformed_records, 1);
  }
}
