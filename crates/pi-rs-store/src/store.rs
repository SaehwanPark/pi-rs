//! The durable boundary: one façade over session state, trace journal, and
//! blobs.
//!
//! The runtime should not know about paths, redaction, or buffering rules. It
//! announces semantic facts, and this crate decides what is written, where, and
//! whether it is durable yet. That placement is deliberate: redaction applied at
//! call sites is optional, and an optional safety rule is one environment
//! variable away from being absent.
//!
//! ```text
//! Store
//!  └─ Session  (one per open session)
//!       ├─ SessionLog    -> sessions/<id>.jsonl        semantic state, resume needs this
//!       ├─ TraceJournal  -> sessions/<id>.trace.jsonl  canonical, ordered, redacted
//!       └─ BlobStore     -> sessions/<id>/blobs/       full payloads behind reductions
//! ```

use std::path::{Path, PathBuf};

use pi_rs_core::{
  capability::ModelRef,
  context::ContextCapsule,
  event::EventEnvelope,
  ids::{CheckpointId, EventSeq, SessionId, TurnId},
  message::Message,
  redact::RedactionPolicy,
  session::{
    SessionCheckpointRecord, SessionHeader, SessionMessage, SessionRecord, SessionSummary,
  },
  trace::{BlobRef, RawPayloadCapture, TraceRetention},
};

use crate::{
  StateLayout, StoreError,
  blob::BlobStore,
  journal::TraceJournal,
  retention::{self, RetentionReport},
  session_log::{self, RestoredSession, SessionLog},
};

/// How large a payload may be before it is stored out of line.
pub const DEFAULT_INLINE_THRESHOLD_BYTES: u64 = 8 * 1024;

/// Write policy for one session, resolved from configuration once.
///
/// Bundling these into the store means a session cannot be opened with weaker
/// protection than the runtime was configured with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WritePolicy {
  pub redaction: RedactionPolicy,
  pub raw_payload: RawPayloadCapture,
  /// Inline budget for one journal line.
  ///
  /// A line that would exceed it has its largest fields stored in the session's
  /// blob store, keeping a preview and an `externalized` record. The budget is
  /// per line rather than per field because the unit a reader pays for — `grep`,
  /// `tail`, a resume that only needs the last few events — is the line.
  pub inline_threshold_bytes: u64,
}

impl Default for WritePolicy {
  fn default() -> Self {
    Self {
      redaction: RedactionPolicy::default(),
      raw_payload: RawPayloadCapture::Disabled,
      inline_threshold_bytes: DEFAULT_INLINE_THRESHOLD_BYTES,
    }
  }
}

impl WritePolicy {
  /// Derive the write policy from durable-state configuration.
  ///
  /// The redaction policy is a separate argument because it comes from the
  /// security section of the configuration, while retention comes from the
  /// storage section; conflating them would make it possible to configure a
  /// retention bound and silently lose the redaction rules with it.
  pub fn from_retention(retention: &TraceRetention, redaction: &RedactionPolicy) -> Self {
    Self {
      redaction: redaction.clone(),
      raw_payload: retention.raw_payload,
      inline_threshold_bytes: retention.inline_threshold_bytes,
    }
  }
}

/// A payload stored for later recovery, in whichever form fits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Payload {
  /// Small enough to live inline; no indirection needed to read it back.
  Inline(String),
  /// Stored content-addressed under the session's blob directory.
  Blob(BlobRef),
}

impl Payload {
  pub fn is_blob(&self) -> bool {
    matches!(self, Self::Blob(_))
  }

  /// Reference suitable for a durable event field.
  pub fn blob(&self) -> Option<&BlobRef> {
    match self {
      Self::Inline(_) => None,
      Self::Blob(blob) => Some(blob),
    }
  }
}

/// An opened state directory.
#[derive(Debug)]
pub struct Store {
  layout: StateLayout,
  policy: WritePolicy,
}

impl Store {
  /// Describe a state root without touching the filesystem.
  pub fn new(root: impl Into<PathBuf>, policy: WritePolicy) -> Self {
    Self {
      layout: StateLayout::new(root),
      policy,
    }
  }

  /// Open the state root, creating the directory structure.
  ///
  /// Called once at startup: one bounded directory-creation pass guarantees the
  /// first append never races a missing directory, without scanning anything.
  pub fn open(root: impl Into<PathBuf>, policy: WritePolicy) -> Result<Self, StoreError> {
    let store = Self::new(root, policy);
    store.layout.create()?;
    Ok(store)
  }

  pub fn layout(&self) -> &StateLayout {
    &self.layout
  }

  pub fn policy(&self) -> &WritePolicy {
    &self.policy
  }

  pub fn root(&self) -> &Path {
    self.layout.root()
  }

  /// Start a new session.
  pub fn begin(&self, header: SessionHeader) -> Result<Session, StoreError> {
    StateLayout::validate_session_id(&header.session_id)?;
    self.layout.ensure_session_dirs(&header.session_id)?;
    let journal = self.open_journal(&header.session_id)?;
    let log = SessionLog::create_with_policy(
      &self.layout.session_path(&header.session_id),
      header.clone(),
      self.policy.redaction.clone(),
    )?;
    Ok(self.session(header, log, journal))
  }

  /// Continue an existing session, appending to both of its logs.
  pub fn resume(&self, session: &SessionId) -> Result<Session, StoreError> {
    let header = SessionLog::read_header(&self.layout.session_path(session))?;
    let journal = self.open_journal(session)?;
    let log = SessionLog::resume_with_policy(
      &self.layout.session_path(session),
      self.policy.redaction.clone(),
    )?;
    Ok(self.session(header, log, journal))
  }

  fn open_journal(&self, session: &SessionId) -> Result<TraceJournal, StoreError> {
    TraceJournal::open(
      &self.layout.trace_path(session),
      self.policy.redaction.clone(),
      self.policy.raw_payload,
    )
  }

  fn session(&self, header: SessionHeader, log: SessionLog, journal: TraceJournal) -> Session {
    Session {
      blobs: BlobStore::for_session(&self.layout, &header.session_id)
        .expect("blob directory was created with the session"),
      header,
      layout: self.layout.clone(),
      log,
      journal,
      policy: self.policy.clone(),
    }
  }

  /// Read what is needed to continue, without opening writers.
  ///
  /// Resume cost is bounded by the checkpoint barrier, not by total session
  /// length: messages before the latest barrier are summarized inside the
  /// capsule, so reading them would be pure waste.
  pub fn restore(&self, session: &SessionId) -> Result<RestoredSession, StoreError> {
    let mut restored = session_log::restore(&self.layout.session_path(session))?;
    let from_trace = TraceJournal::read_tail(&self.layout.trace_path(session))?
      .items
      .iter()
      .filter_map(|entry| entry.envelope.meta.seq)
      .max();
    restored.last_seq = match (restored.last_seq, from_trace) {
      (Some(a), Some(b)) => Some(EventSeq(a.0.max(b.0))),
      (Some(a), None) => Some(a),
      (None, Some(b)) => Some(b),
      (None, None) => None,
    };
    Ok(restored)
  }

  pub fn exists(&self, session: &SessionId) -> bool {
    self.layout.session_path(session).exists()
  }

  /// Cheap session list, newest first: headers and trace tails only.
  ///
  /// Message counts are intentionally zero here. Counting them means reading
  /// every message body of every session, which is the classic way
  /// `list sessions` becomes the slowest command in the tool. Use
  /// [`Store::details`] when the user asked about one specific session.
  pub fn summaries(&self, limit: usize) -> Result<Vec<SessionSummary>, StoreError> {
    let mut summaries = Vec::new();
    for id in self.layout.list_session_ids()? {
      if summaries.len() >= limit {
        break;
      }
      summaries.push(SessionLog::summary(
        &self.layout.session_path(&id),
        &self.layout.trace_path(&id),
      )?);
    }
    Ok(summaries)
  }

  /// Full summary for one session, including counts and a preview.
  pub fn details(
    &self,
    session: &SessionId,
    preview_chars: usize,
  ) -> Result<SessionSummary, StoreError> {
    SessionLog::summary_report(
      &self.layout.session_path(session),
      &self.layout.trace_path(session),
      preview_chars,
    )
  }

  pub fn blobs(&self, session: &SessionId) -> Result<BlobStore, StoreError> {
    BlobStore::for_session(&self.layout, session)
  }

  /// Total bytes held under the sessions directory.
  pub fn used_bytes(&self) -> Result<u64, StoreError> {
    self.layout.state_bytes()
  }

  pub fn remove(&self, session: &SessionId) -> Result<u64, StoreError> {
    let freed = self.layout.session_bytes(session)?;
    self.layout.remove_session(session)?;
    Ok(freed)
  }

  /// Report what the configured retention would delete, deleting nothing.
  ///
  /// Retention destroys data that cannot be recovered afterwards, so the plan is
  /// a first-class operation rather than a log line the user has to trust.
  pub fn plan_retention(
    &self,
    retention: &TraceRetention,
    now_ms: u64,
    keep_newest: usize,
  ) -> Result<RetentionReport, StoreError> {
    retention::dry_run(&self.layout, retention, now_ms, keep_newest)
  }

  /// Delete expired and over-cap sessions.
  pub fn apply_retention(
    &self,
    retention: &TraceRetention,
    now_ms: u64,
    keep_newest: usize,
  ) -> Result<RetentionReport, StoreError> {
    retention::apply(&self.layout, retention, now_ms, keep_newest)
  }
}

/// One open session: the runtime's only durable handle.
#[derive(Debug)]
pub struct Session {
  header: SessionHeader,
  layout: StateLayout,
  log: SessionLog,
  journal: TraceJournal,
  blobs: BlobStore,
  policy: WritePolicy,
}

impl Session {
  pub fn id(&self) -> &SessionId {
    &self.header.session_id
  }

  pub fn header(&self) -> &SessionHeader {
    &self.header
  }

  pub fn path(&self) -> &Path {
    self.log.path()
  }

  pub fn trace_path(&self) -> &Path {
    self.journal.path()
  }

  pub fn records(&self) -> usize {
    self.log.records()
  }

  pub fn last_seq(&self) -> Option<EventSeq> {
    self.journal.last_seq()
  }

  /// Sequence the next event will receive.
  pub fn next_seq(&self) -> EventSeq {
    self.journal.next_seq()
  }

  pub fn blobs(&self) -> &BlobStore {
    &self.blobs
  }

  pub fn policy(&self) -> &WritePolicy {
    &self.policy
  }

  /// Append a canonical trace event, stamping the authoritative sequence number
  /// back into the caller's envelope.
  ///
  /// The log owns ordering; writing the assigned number into the envelope keeps
  /// the caller's in-memory event identical to the line on disk, which is what
  /// lets a session record point at its trace event without guessing.
  pub fn emit(&mut self, envelope: &mut EventEnvelope) -> Result<EventSeq, StoreError> {
    let seq = self.journal.append_bounded(
      envelope,
      None,
      Some(&self.blobs),
      self.policy.inline_threshold_bytes,
    )?;
    envelope.meta.seq = Some(seq);
    Ok(seq)
  }

  /// Append a trace event together with the raw bytes that produced it.
  ///
  /// When raw capture is disabled the bytes are dropped without an error, and no
  /// recovery pointer is recorded. The caller's intent is expressed by policy,
  /// so a caller that stored a payload anyway cannot make it reachable through a
  /// quiet line.
  pub fn emit_with_payload(
    &mut self,
    envelope: &mut EventEnvelope,
    raw: &[u8],
  ) -> Result<EventSeq, StoreError> {
    if !self.policy.raw_payload.is_enabled() {
      return self.emit(envelope);
    }
    let blob = self.blobs.put(raw, None)?;
    let relative = self.layout.blob_relative_path(&blob);
    // The captured body is already in a blob; the line that points at it is still
    // held to the same inline budget as every other line.
    let seq = self.journal.append_bounded(
      envelope,
      Some(&relative),
      Some(&self.blobs),
      self.policy.inline_threshold_bytes,
    )?;
    envelope.meta.seq = Some(seq);
    Ok(seq)
  }

  /// Append a semantic session record.
  pub fn record(&mut self, record: &SessionRecord) -> Result<(), StoreError> {
    self.log.append(record)
  }

  /// Persist one message with its model attribution, bound to the event that
  /// introduced it.
  ///
  /// Attribution is not decoration: without the epoch and model that produced a
  /// message, a session cannot say which model is responsible for a claim after
  /// a failover.
  ///
  /// `envelope` must already have been emitted, so that its sequence number is
  /// the journal's rather than the caller's guess.
  pub fn append_message(
    &mut self,
    turn_id: &TurnId,
    message: &Message,
    epoch: u32,
    model: &ModelRef,
    envelope: &EventEnvelope,
  ) -> Result<SessionMessage, StoreError> {
    let seq = envelope.meta.seq.ok_or_else(|| {
      StoreError::Invalid(
        "the event introducing a message must be emitted first, so that the session record \
         points at a real journal position"
          .into(),
      )
    })?;
    let record = SessionMessage {
      turn_id: turn_id.clone(),
      role: message.role,
      message: message.clone(),
      epoch,
      model: model.clone(),
      event_id: envelope.meta.event_id.clone(),
      seq: Some(seq),
    };
    self.log.append(&SessionRecord::Message(record.clone()))?;
    Ok(record)
  }

  /// Write a checkpoint capsule, append its barrier, and return the record.
  ///
  /// The caller still emits `CheckpointCreated`: the store does not invent trace
  /// events, because the runtime owns event identity and turn structure.
  ///
  /// The capsule is written before the barrier on purpose. A barrier pointing at
  /// a missing capsule breaks resume, while an unreferenced capsule is merely an
  /// orphan that retention later removes.
  pub fn checkpoint(
    &mut self,
    capsule: &ContextCapsule,
  ) -> Result<SessionCheckpointRecord, StoreError> {
    let checkpoint_id = CheckpointId::new();
    let path = self.layout.checkpoint_path(self.id(), &checkpoint_id);
    if let Some(dir) = path.parent() {
      std::fs::create_dir_all(dir)?;
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(capsule)?)?;
    std::fs::rename(&temporary, &path)?;

    let record = SessionCheckpointRecord {
      capsule_path: format!("checkpoints/{checkpoint_id}.json"),
      checkpoint_id,
      capsule_version: capsule.version,
      capsule: capsule.clone(),
    };
    self
      .log
      .append(&SessionRecord::CheckpointBarrier(record.clone()))?;
    Ok(record)
  }

  /// Store a payload, redacting it at this boundary before choosing inline or
  /// blob storage by the configured threshold.
  pub fn put_payload(&mut self, bytes: &[u8]) -> Result<Payload, StoreError> {
    let redacted = self.policy.redaction.apply(&String::from_utf8_lossy(bytes));
    let bytes = redacted.text.as_bytes();
    if (bytes.len() as u64) < self.policy.inline_threshold_bytes {
      return Ok(Payload::Inline(redacted.text));
    }
    Ok(Payload::Blob(self.blobs.put(bytes, None)?))
  }

  /// Store recovery bytes after applying the configured durable redaction policy.
  ///
  /// Unlike [`Self::put_payload`], this always returns a blob because runtime
  /// reduction events need a stable recovery reference even for a small payload.
  pub fn put_recovery_blob(&self, bytes: &[u8]) -> Result<BlobRef, StoreError> {
    let redacted = self.policy.redaction.apply(&String::from_utf8_lossy(bytes));
    self.blobs.put(redacted.text.as_bytes(), None)
  }

  /// Flush both logs.
  ///
  /// Streaming-only progress uses this instead of forcing a sync for every
  /// delta; state-changing events are already durable.
  pub fn flush(&mut self) -> Result<(), StoreError> {
    self.journal.flush()?;
    self.log.flush()
  }

  /// Finish with the session: flush and release appenders.
  pub fn finish(mut self) -> Result<(), StoreError> {
    self.flush()
  }
}

#[cfg(test)]
mod tests {
  use pi_rs_core::{
    capability::EpochReason,
    context::CAPSULE_SCHEMA_VERSION,
    event::{
      AgentEvent, CheckpointCreated, Diagnostic, DiagnosticLevel, EventMeta, SessionEndReason,
      SessionEnded, ToolRequested, TurnCompleted, TurnStatus, UserMessage,
    },
    ids::{EventId, ToolCallId, TraceId, uuidv7},
    session::{SESSION_SCHEMA_VERSION, SessionEpochRecord},
    trace::TraceRetention,
  };

  use crate::{StateLayout, tmp::TempDir};

  use super::*;

  fn store(tmp: &TempDir) -> Store {
    Store::open(tmp.path(), WritePolicy::default()).unwrap()
  }

  fn header(id: &SessionId) -> SessionHeader {
    SessionHeader {
      session_id: id.clone(),
      version: SESSION_SCHEMA_VERSION,
      started_at_ms: 1_700_000_000_000,
      working_dir: "/repo".into(),
      model: ModelRef::new("local", "qwen"),
      parent_session: None,
      branched_from_event: None,
      imported_from: None,
    }
  }

  fn meta(session: &SessionId, turn: &TurnId) -> EventMeta {
    EventMeta {
      event_id: EventId::new(),
      session_id: session.clone(),
      turn_id: Some(turn.clone()),
      seq: None,
      timestamp_ms: 1,
      model_epoch: Some(0),
      model: Some(ModelRef::new("local", "qwen")),
      tool_call_id: None,
      parent_event_id: None,
      trace_id: TraceId::new(),
      span_id: pi_rs_core::ids::SpanId::new(),
    }
  }

  fn turn_done(session: &SessionId, turn: &TurnId) -> EventEnvelope {
    EventEnvelope::new(
      meta(session, turn),
      AgentEvent::TurnCompleted(TurnCompleted {
        status: TurnStatus::Completed,
        duration_ms: 5,
      }),
    )
  }

  fn capsule(objective: &str) -> ContextCapsule {
    ContextCapsule {
      version: CAPSULE_SCHEMA_VERSION,
      objective: objective.into(),
      completed_work: vec!["journal".into()],
      decisions: vec![],
      constraints: vec!["no tokio".into()],
      current_state: "writing tests".into(),
      artifacts: vec![],
      unresolved: vec!["provider phase".into()],
      next_actions: vec!["cargo test".into()],
    }
  }

  #[test]
  fn begin_makes_the_session_listable_immediately() {
    let tmp = TempDir::new("store-begin");
    let opened = store(&tmp);
    let id = SessionId::from_string(uuidv7());
    let session = opened.begin(header(&id)).unwrap();
    assert!(
      session.path().exists(),
      "header is written through at create"
    );
    assert_eq!(
      std::fs::metadata(session.trace_path())
        .map(|meta| meta.len())
        .unwrap_or(0),
      0,
      "the trace file may exist, but holds no records before an event"
    );
    assert_eq!(session.last_seq(), None);
    assert_eq!(session.next_seq(), EventSeq(1));
    assert!(opened.exists(&id));
    let listed = opened.summaries(10).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].session_id, id);
    // The state directory is private, because trace data may hold secrets.
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      let mode = std::fs::metadata(opened.root())
        .unwrap()
        .permissions()
        .mode();
      assert_eq!(
        mode & 0o077,
        0,
        "state root must not be group or other accessible"
      );
    }
  }

  #[test]
  fn emit_stamps_the_authoritative_sequence_into_the_caller_envelope() {
    let tmp = TempDir::new("store-stamp");
    let opened = store(&tmp);
    let id = SessionId::from_string(uuidv7());
    let turn = TurnId::new();
    let mut session = opened.begin(header(&id)).unwrap();
    let mut first = turn_done(&id, &turn);
    let before = first.meta.event_id.clone();
    assert_eq!(session.emit(&mut first).unwrap(), EventSeq(1));
    assert_eq!(first.meta.seq, Some(EventSeq(1)));
    assert_eq!(
      first.meta.event_id, before,
      "identity is the caller's, order is the log's"
    );
    assert_eq!(session.last_seq(), Some(EventSeq(1)));
    assert!(session.trace_path().exists());
  }

  /// A model asking to write a large file is the ordinary way a turn produces a
  /// field far larger than a line should hold.
  fn huge_write(id: &SessionId, turn: &TurnId, contents: &str) -> EventEnvelope {
    EventEnvelope::new(
      meta(id, turn),
      AgentEvent::ToolRequested(ToolRequested {
        call_id: ToolCallId::new(),
        name: "write".into(),
        arguments: serde_json::json!({"path": "generated/data.bin", "contents": contents}),
        read_only: false,
      }),
    )
  }

  fn last_line(path: &Path) -> String {
    std::fs::read_to_string(path)
      .unwrap()
      .lines()
      .last()
      .map(str::to_string)
      .unwrap()
  }

  #[test]
  fn an_oversized_event_is_written_within_the_inline_budget() {
    let tmp = TempDir::new("store-bounded-line");
    let opened = store(&tmp);
    let id = SessionId::from_string(uuidv7());
    let turn = TurnId::new();
    let mut session = opened.begin(header(&id)).unwrap();
    let contents = "x".repeat(200 * 1024);
    let mut envelope = huge_write(&id, &turn, &contents);
    assert_eq!(session.emit(&mut envelope).unwrap(), EventSeq(1));

    let line = last_line(session.trace_path());
    assert!(
      line.len() <= DEFAULT_INLINE_THRESHOLD_BYTES as usize,
      "a 200 KiB argument must not become a 200 KiB line: {} bytes",
      line.len()
    );
    let report = TraceJournal::read(session.trace_path()).unwrap();
    assert_eq!(report.malformed, 0);
    let entry = &report.items[0];
    assert_eq!(entry.externalized.len(), 1);
    assert_eq!(entry.externalized[0].field, "arguments/contents");
    assert_eq!(entry.externalized[0].bytes, contents.len() as u64);
    assert!(
      line.contains("generated/data.bin"),
      "the request is still identifiable"
    );

    // The bytes are reachable from the recorded reference alone.
    assert_eq!(
      session
        .blobs()
        .get_relative(&entry.externalized[0].reference)
        .unwrap(),
      contents.as_bytes()
    );
    session.finish().unwrap();
  }

  #[test]
  fn bounding_does_not_disturb_the_surfaces_that_resume_depends_on() {
    // The session log holds the semantic state resume needs; bounding applies to
    // journal lines, so a bounded event must leave resume, listing, and byte
    // accounting behaving as they would have otherwise.
    let tmp = TempDir::new("store-bounded-resume");
    let opened = store(&tmp);
    let id = SessionId::from_string(uuidv7());
    let turn = TurnId::new();
    let mut session = opened.begin(header(&id)).unwrap();
    let contents = "y".repeat(64 * 1024);
    session
      .emit(&mut huge_write(&id, &turn, &contents))
      .unwrap();
    let mut introduced = EventEnvelope::new(
      meta(&id, &turn),
      AgentEvent::UserMessage(UserMessage {
        text: "write the file".into(),
        attachments: 0,
      }),
    );
    session.emit(&mut introduced).unwrap();
    session
      .append_message(
        &turn,
        &pi_rs_core::message::Message::user("write the file"),
        0,
        &ModelRef::new("local", "qwen"),
        &introduced,
      )
      .unwrap();
    session.finish().unwrap();

    let reopened = Store::open(tmp.path(), WritePolicy::default()).unwrap();
    let listed = reopened.summaries(10).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].session_id, id);
    let resumed = reopened.resume(&id).unwrap();
    assert_eq!(
      resumed.last_seq(),
      Some(EventSeq(2)),
      "order survives bounding"
    );
    let report = TraceJournal::read(resumed.trace_path()).unwrap();
    assert_eq!(report.items.len(), 2);
    assert_eq!(report.items[0].externalized.len(), 1);
    assert_eq!(
      report.items[1].externalized.len(),
      0,
      "an ordinary line pays nothing"
    );
    assert!(
      reopened.used_bytes().unwrap() >= contents.len() as u64,
      "spilled bytes are counted as used"
    );
  }

  #[test]
  fn a_message_record_points_at_a_real_journal_position() {
    let tmp = TempDir::new("store-message");
    let opened = store(&tmp);
    let id = SessionId::from_string(uuidv7());
    let turn = TurnId::new();
    let mut session = opened.begin(header(&id)).unwrap();
    let mut introduced = EventEnvelope::new(
      meta(&id, &turn),
      AgentEvent::UserMessage(UserMessage {
        text: "hello".into(),
        attachments: 0,
      }),
    );
    let error = session
      .append_message(
        &turn,
        &pi_rs_core::message::Message::user("hello"),
        0,
        &ModelRef::new("local", "qwen"),
        &introduced,
      )
      .unwrap_err();
    assert!(matches!(error, StoreError::Invalid(_)), "{error}");

    session.emit(&mut introduced).unwrap();
    let record = session
      .append_message(
        &turn,
        &pi_rs_core::message::Message::user("hello"),
        0,
        &ModelRef::new("local", "qwen"),
        &introduced,
      )
      .unwrap();
    assert_eq!(record.seq, Some(EventSeq(1)));
    assert_eq!(record.event_id, introduced.meta.event_id);
    assert_eq!(record.role, pi_rs_core::message::Role::User);
  }

  #[test]
  fn resume_continues_the_journal_and_keeps_the_header() {
    let tmp = TempDir::new("store-resume");
    let opened = store(&tmp);
    let id = SessionId::from_string(uuidv7());
    let turn = TurnId::new();
    {
      let mut session = opened.begin(header(&id)).unwrap();
      for _ in 0..3 {
        session.emit(&mut turn_done(&id, &turn)).unwrap();
      }
      session.flush().unwrap();
    }
    let mut resumed = opened.resume(&id).unwrap();
    assert_eq!(resumed.header().session_id, id);
    assert_eq!(resumed.last_seq(), Some(EventSeq(3)));
    let mut next = turn_done(&id, &turn);
    assert_eq!(resumed.emit(&mut next).unwrap(), EventSeq(4));
    assert_eq!(
      resumed.records(),
      1,
      "the session log still holds header only"
    );
  }

  #[test]
  fn checkpoint_resume_reads_a_bounded_window() {
    let tmp = TempDir::new("store-checkpoint");
    let opened = store(&tmp);
    let id = SessionId::from_string(uuidv7());
    let turn = TurnId::new();
    {
      let mut session = opened.begin(header(&id)).unwrap();
      for index in 0..4u32 {
        let mut introduced = EventEnvelope::new(
          meta(&id, &turn),
          AgentEvent::UserMessage(UserMessage {
            text: format!("long message {index}"),
            attachments: 0,
          }),
        );
        session.emit(&mut introduced).unwrap();
        session
          .append_message(
            &turn,
            &pi_rs_core::message::Message::user(format!("long message {index}")),
            0,
            &ModelRef::new("local", "qwen"),
            &introduced,
          )
          .unwrap();
      }
      let barrier = session.checkpoint(&capsule("ship the store")).unwrap();
      let mut created = EventEnvelope::new(
        meta(&id, &turn),
        AgentEvent::CheckpointCreated(CheckpointCreated {
          checkpoint_id: barrier.checkpoint_id.clone(),
          capsule_version: barrier.capsule_version,
          summarized_events: 4,
          path: barrier.capsule_path.clone(),
        }),
      );
      session.emit(&mut created).unwrap();
      let mut after = EventEnvelope::new(
        meta(&id, &turn),
        AgentEvent::UserMessage(UserMessage {
          text: "after".into(),
          attachments: 0,
        }),
      );
      session.emit(&mut after).unwrap();
      session
        .append_message(
          &turn,
          &pi_rs_core::message::Message::user("after"),
          0,
          &ModelRef::new("local", "qwen"),
          &after,
        )
        .unwrap();
      assert_eq!(
        barrier.capsule_path,
        format!("checkpoints/{}.json", barrier.checkpoint_id)
      );
      session.finish().unwrap();
    }

    let restored = opened.restore(&id).unwrap();
    assert_eq!(restored.summarized_messages, 4);
    assert_eq!(restored.messages.len(), 1);
    assert_eq!(restored.messages[0].message.text(), "after");
    assert_eq!(
      restored.checkpoint.as_ref().unwrap().objective,
      "ship the store"
    );
    assert_eq!(restored.checkpoint_seq, Some(EventSeq(4)));
    assert_eq!(
      restored.last_seq,
      Some(EventSeq(6)),
      "trace tail is authoritative"
    );
    assert_eq!(restored.malformed_records, 0);

    // The capsule is readable standalone, not only through the barrier.
    let files = std::fs::read_dir(opened.layout().checkpoints_dir(&id))
      .unwrap()
      .filter_map(Result::ok)
      .collect::<Vec<_>>();
    assert_eq!(files.len(), 1);
    let stored: ContextCapsule =
      serde_json::from_slice(&std::fs::read(files[0].path()).unwrap()).unwrap();
    assert_eq!(stored.constraints, vec!["no tokio".to_string()]);
  }

  #[test]
  fn epoch_records_survive_resume() {
    let tmp = TempDir::new("store-epoch");
    let opened = store(&tmp);
    let id = SessionId::from_string(uuidv7());
    let mut session = opened.begin(header(&id)).unwrap();
    session
      .record(&SessionRecord::Epoch(SessionEpochRecord {
        epoch: 1,
        model: ModelRef::new("backup", "small"),
        reason: EpochReason::AutomaticFailover,
      }))
      .unwrap();
    session.finish().unwrap();
    let restored = opened.restore(&id).unwrap();
    assert_eq!(restored.epochs.len(), 1);
    assert_eq!(restored.epochs[0].model, ModelRef::new("backup", "small"));
    assert_eq!(restored.epochs[0].reason, EpochReason::AutomaticFailover);
  }

  #[test]
  fn raw_capture_is_off_by_default_and_never_inlines_bytes() {
    let tmp = TempDir::new("store-raw");
    let id = SessionId::from_string(uuidv7());
    let turn = TurnId::new();
    let opened = store(&tmp);
    let mut session = opened.begin(header(&id)).unwrap();
    session
      .emit_with_payload(
        &mut turn_done(&id, &turn),
        b"{\"api_key\":\"secret-value\"}",
      )
      .unwrap();
    session.finish().unwrap();
    let on_disk = std::fs::read_to_string(opened.layout().trace_path(&id)).unwrap();
    assert!(!on_disk.contains("raw_ref"), "opt-in only: {on_disk}");
    assert!(!on_disk.contains("secret-value"), "raw bytes are dropped");

    let enabled = Store::open(
      tmp.path().join("enabled"),
      WritePolicy {
        raw_payload: RawPayloadCapture::Enabled,
        ..WritePolicy::default()
      },
    )
    .unwrap();
    let other = SessionId::from_string(uuidv7());
    let mut session = enabled.begin(header(&other)).unwrap();
    session
      .emit_with_payload(
        &mut turn_done(&other, &turn),
        b"{\"api_key\":\"secret-value\"}",
      )
      .unwrap();
    session.finish().unwrap();
    let on_disk = std::fs::read_to_string(enabled.layout().trace_path(&other)).unwrap();
    assert!(
      on_disk.contains("raw_ref"),
      "a pointer is recorded: {on_disk}"
    );
    assert!(
      !on_disk.contains("secret-value"),
      "the payload stays in the blob store"
    );
    let captured = objects_under(&enabled.layout().blobs_dir(&other));
    assert_eq!(
      captured,
      vec![b"{\"api_key\":\"secret-value\"}".to_vec()],
      "captured bytes are stored out of line under the session blob directory"
    );
  }

  /// Every stored object below a directory, descending into hash shards.
  fn objects_under(dir: &std::path::Path) -> Vec<Vec<u8>> {
    let mut objects = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap().filter_map(Result::ok) {
      if entry.path().is_file() {
        objects.push(std::fs::read(entry.path()).unwrap());
      } else {
        objects.extend(objects_under(&entry.path()));
      }
    }
    objects.sort();
    objects
  }

  #[test]
  fn secrets_are_redacted_at_the_boundary_not_at_call_sites() {
    let tmp = TempDir::new("store-redact");
    let opened = Store::open(
      tmp.path(),
      WritePolicy {
        redaction: RedactionPolicy::default(),
        ..WritePolicy::default()
      },
    )
    .unwrap();
    let id = SessionId::from_string(uuidv7());
    let turn = TurnId::new();
    let mut session = opened.begin(header(&id)).unwrap();
    session
      .emit(&mut EventEnvelope::new(
        meta(&id, &turn),
        AgentEvent::Diagnostic(Diagnostic {
          level: DiagnosticLevel::Warn,
          message: "auth failed api_key=sk-abcdefghijklmnop".into(),
        }),
      ))
      .unwrap();
    session.finish().unwrap();
    let on_disk = std::fs::read_to_string(opened.layout().trace_path(&id)).unwrap();
    assert!(!on_disk.contains("sk-abcdefghijklmnop"), "{on_disk}");
    assert!(on_disk.contains("redacted"), "{on_disk}");
  }

  #[test]
  fn payload_size_decides_inline_or_blob() {
    let tmp = TempDir::new("store-payload");
    let opened = store(&tmp);
    let id = SessionId::from_string(uuidv7());
    let mut session = opened.begin(header(&id)).unwrap();
    assert_eq!(
      session.put_payload(b"short".as_slice()).unwrap(),
      Payload::Inline("short".into())
    );
    let large = session.put_payload(vec![b'x'; 9_000].as_slice()).unwrap();
    assert!(large.is_blob());
    assert_eq!(
      session.blobs().get(large.blob().unwrap()).unwrap().len(),
      9_000
    );
    let reference = large.blob().unwrap();
    assert!(
      session.blobs().verify(reference).unwrap(),
      "stored bytes match their address"
    );
    assert_eq!(session.last_seq(), None, "a payload is not an event");
  }

  #[test]
  fn listing_is_cheap_and_details_are_detailed() {
    let tmp = TempDir::new("store-details");
    let opened = store(&tmp);
    let id = SessionId::from_string(uuidv7());
    let turn = TurnId::new();
    let mut session = opened.begin(header(&id)).unwrap();
    let mut introduced = EventEnvelope::new(
      meta(&id, &turn),
      AgentEvent::UserMessage(UserMessage {
        text: "implement the durable store for pi-rs, including journals and blobs".into(),
        attachments: 0,
      }),
    );
    session.emit(&mut introduced).unwrap();
    session
      .append_message(
        &turn,
        &pi_rs_core::message::Message::user(
          "implement the durable store for pi-rs, including journals and blobs",
        ),
        0,
        &ModelRef::new("local", "qwen"),
        &introduced,
      )
      .unwrap();
    session
      .record(&SessionRecord::Epoch(SessionEpochRecord {
        epoch: 1,
        model: ModelRef::new("backup", "small"),
        reason: EpochReason::AutomaticFailover,
      }))
      .unwrap();
    session
      .emit(&mut EventEnvelope::new(
        meta(&id, &turn),
        AgentEvent::SessionEnded(SessionEnded {
          reason: SessionEndReason::UserExit,
        }),
      ))
      .unwrap();
    session.finish().unwrap();

    let cheap = opened.summaries(10).unwrap();
    assert_eq!(cheap.len(), 1);
    assert!(
      cheap[0].closed,
      "a session end in the trace tail is visible"
    );
    assert_eq!(cheap[0].messages, 0, "cheap listing does not read bodies");
    assert_eq!(cheap[0].model, ModelRef::new("local", "qwen"));

    let detailed = opened.details(&id, 24).unwrap();
    assert_eq!(detailed.messages, 1);
    assert_eq!(detailed.last_model, Some(ModelRef::new("backup", "small")));
    let preview = detailed.last_turn_preview.unwrap();
    assert!(preview.chars().count() <= 25, "{preview}");
    assert!(preview.ends_with('…'));
  }

  #[test]
  fn retention_can_be_inspected_before_it_destroys() {
    let tmp = TempDir::new("store-retention");
    let opened = store(&tmp);
    let id = SessionId::from_string(uuidv7());
    opened.begin(header(&id)).unwrap().finish().unwrap();
    let config = TraceRetention {
      max_bytes: Some(1),
      ..TraceRetention::default()
    };
    let planned = opened
      .plan_retention(&config, 1_800_000_000_000, 0)
      .unwrap();
    assert_eq!(planned.removed_count(), 1, "{planned:?}");
    assert!(opened.exists(&id), "a plan deletes nothing");
    let applied = opened
      .apply_retention(&config, 1_800_000_000_000, 0)
      .unwrap();
    assert_eq!(applied.freed_bytes, planned.freed_bytes);
    assert!(!opened.exists(&id));
    assert_eq!(opened.used_bytes().unwrap(), 0);
    assert_eq!(opened.summaries(10).unwrap().len(), 0);
  }

  #[test]
  fn policy_thresholds_come_from_configuration() {
    let derived = WritePolicy::from_retention(
      &TraceRetention {
        inline_threshold_bytes: 64,
        raw_payload: RawPayloadCapture::Enabled,
        ..TraceRetention::default()
      },
      &RedactionPolicy::default(),
    );
    assert_eq!(derived.inline_threshold_bytes, 64);
    assert!(derived.raw_payload.is_enabled());
    assert_eq!(derived.redaction, RedactionPolicy::default());

    let tmp = TempDir::new("store-threshold");
    let opened = Store::open(tmp.path(), derived).unwrap();
    let id = SessionId::from_string(uuidv7());
    let mut session = opened.begin(header(&id)).unwrap();
    assert!(
      session
        .put_payload(vec![b'y'; 100].as_slice())
        .unwrap()
        .is_blob(),
      "a configured threshold is honoured"
    );
  }

  #[test]
  fn begin_rejects_a_path_escaping_session_id() {
    let tmp = TempDir::new("store-bad-id");
    let opened = store(&tmp);
    let error = opened
      .begin(header(&SessionId::from_string("../escape")))
      .unwrap_err();
    assert!(matches!(error, StoreError::Invalid(_)), "{error}");
    assert!(!tmp.path().join("../escape.jsonl").exists());
    assert_eq!(opened.summaries(10).unwrap().len(), 0);
  }

  #[test]
  fn resume_reports_a_missing_session_explicitly() {
    let tmp = TempDir::new("store-missing");
    let opened = store(&tmp);
    let error = opened
      .resume(&SessionId::from_string(uuidv7()))
      .unwrap_err();
    assert!(matches!(error, StoreError::Invalid(_)), "{error}");
  }

  #[test]
  fn session_started_ms_matches_the_state_layout_id_rule() {
    let tmp = TempDir::new("store-age");
    let opened = store(&tmp);
    let id = SessionId::from_string(uuidv7());
    opened.begin(header(&id)).unwrap().finish().unwrap();
    let started = crate::retention::session_started_ms(&id).expect("minted ids are dated");
    assert!(StateLayout::validate_session_id(&id).is_ok(), "{started}");
  }
}
