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

use std::{
  collections::{BTreeMap, BTreeSet},
  path::{Path, PathBuf},
};

use rupi_core::{
  capability::ModelRef,
  context::{ContextCapsule, ExternalContextRef},
  event::{
    AgentEvent, Diagnostic, DiagnosticLevel, EventEnvelope, EventMeta, ModelRequestCompleted,
    ToolFailed,
  },
  ids::{CheckpointId, EventId, EventSeq, SessionId, ToolCallId, TurnId},
  message::{ContentBlock, Message, Role, ToolResultBlock},
  redact::RedactionPolicy,
  session::{
    InterruptedToolCall, SessionCheckpointRecord, SessionHeader, SessionMessage, SessionRecord,
    SessionSummary,
  },
  tool::{ToolExecutionState, ToolRequest},
  trace::{BlobCompression, BlobRef, RawPayloadCapture, TraceRetention},
};

use crate::{
  StateLayout, StoreError,
  blob::BlobStore,
  journal::TraceJournal,
  lease::SessionLease,
  projection::{MessageRecovery, ProjectionWal, validate_projection_size},
  retention::{self, RetentionReport},
  session_log::{self, RestoredSession, SessionLog},
};

/// How large a payload may be before it is stored out of line.
pub const DEFAULT_INLINE_THRESHOLD_BYTES: u64 = 8 * 1024;

/// Maximum encoded size of one checkpoint capsule. Capsules are model-visible
/// recovery state, not arbitrary attachments; bounding them also keeps the
/// checkpoint projection WAL within its line limit.
const MAX_CAPSULE_BYTES: u64 = 128 * 1024;
const MAX_CHECKPOINT_COUNT: usize = 1_024;
const MAX_CHECKPOINT_TOTAL_BYTES: u64 = 8 * 1024 * 1024;

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectionFailpoint {
  Prepare,
  CanonicalAppend,
  SemanticAppend,
}

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
  /// Optional encoding preference for payload bytes behind blob references.
  pub compression: BlobCompression,
}

impl Default for WritePolicy {
  fn default() -> Self {
    Self {
      redaction: RedactionPolicy::default(),
      raw_payload: RawPayloadCapture::Disabled,
      inline_threshold_bytes: DEFAULT_INLINE_THRESHOLD_BYTES,
      compression: BlobCompression::None,
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
      compression: retention.compression,
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

  /// Open the durable trust decisions for this state root.
  pub fn trust_store(&self) -> Result<crate::FileTrustStore, StoreError> {
    crate::FileTrustStore::open(self.root())
  }

  /// Start a new session.
  pub fn begin(&self, header: SessionHeader) -> Result<Session, StoreError> {
    StateLayout::validate_session_id(&header.session_id)?;
    self.layout.ensure_session_dirs(&header.session_id)?;
    let lease = SessionLease::acquire(&self.layout.lease_path(&header.session_id))?;
    let journal = self.open_journal(&header.session_id)?;
    let log = SessionLog::create_with_policy(
      &self.layout.session_path(&header.session_id),
      header.clone(),
      self.policy.redaction.clone(),
    )?;
    let wal = self.open_wal(&header.session_id)?;
    Ok(self.session(header, log, journal, wal, lease))
  }

  /// Continue an existing session, appending to both of its logs.
  pub fn resume(&self, session: &SessionId) -> Result<Session, StoreError> {
    StateLayout::validate_session_id(session)?;
    let header = SessionLog::read_header(&self.layout.session_path(session))?;
    if header.session_id != *session {
      return Err(StoreError::Invalid(format!(
        "session path {} contains header for {}; resume requires recovery",
        session, header.session_id
      )));
    }
    self.layout.ensure_session_dirs(session)?;
    let lease = SessionLease::acquire(&self.layout.lease_path(session))?;
    // Legacy semantic logs are upgraded while the lease is held and before any
    // append handle exists. This prevents a v1/v2 header from claiming a file
    // that now contains v3-only reduction or checkpoint records.
    SessionLog::migrate_to_current(
      &self.layout.session_path(session),
      self.policy.redaction.clone(),
    )?;
    let header = SessionLog::read_header(&self.layout.session_path(session))?;
    let session_report = SessionLog::read(&self.layout.session_path(session))?;
    if session_report.malformed > 0 {
      return Err(StoreError::Invalid(format!(
        "session {session} contains {} malformed semantic record(s); resume is unsafe",
        session_report.malformed
      )));
    }
    let journal = self.open_journal(session)?;
    if journal.malformed_lines() > 0 {
      return Err(StoreError::Invalid(format!(
        "session {session} trace contains {} malformed record(s); resume is unsafe",
        journal.malformed_lines()
      )));
    }
    let log = SessionLog::resume_with_policy(
      &self.layout.session_path(session),
      self.policy.redaction.clone(),
    )?;
    let wal = self.open_wal(session)?;
    let mut opened = self.session(header, log, journal, wal, lease);
    opened.recover_projection()?;
    opened.recover_abandoned_model_requests()?;
    opened.recover_unrequested_assistant_calls()?;
    opened.recover_unstarted_tool_requests()?;
    // Recovery may have repaired a WAL intent or normalized a safe interrupted
    // lifecycle; validate the complete state before returning an append handle
    // so callers cannot issue a provider request from a projection that still
    // disagrees with canonical history.
    Store::new(self.layout.root().to_path_buf(), self.policy.clone()).restore(session)?;
    Ok(opened)
  }

  fn open_journal(&self, session: &SessionId) -> Result<TraceJournal, StoreError> {
    TraceJournal::open(
      &self.layout.trace_path(session),
      self.policy.redaction.clone(),
      self.policy.raw_payload,
    )
  }

  fn open_wal(&self, session: &SessionId) -> Result<ProjectionWal, StoreError> {
    ProjectionWal::open(
      &self.layout.wal_path(session),
      self.policy.redaction.clone(),
    )
  }

  fn session(
    &self,
    header: SessionHeader,
    log: SessionLog,
    journal: TraceJournal,
    wal: ProjectionWal,
    lease: SessionLease,
  ) -> Session {
    Session {
      blobs: BlobStore::for_session_with_compression(
        &self.layout,
        &header.session_id,
        self.policy.compression,
      )
      .expect("blob directory was created with the session"),
      header,
      layout: self.layout.clone(),
      log,
      journal,
      wal,
      _lease: lease,
      policy: self.policy.clone(),
      #[cfg(test)]
      failpoint: None,
    }
  }

  /// Read what is needed to continue, without opening writers.
  ///
  /// Model-visible reconstruction is bounded by the latest checkpoint barrier:
  /// messages before it are summarized inside the capsule. The integrity and
  /// lifecycle pass still scans canonical history, because skipping an older
  /// unresolved tool or projection would make fail-closed resume unsound.
  pub fn restore(&self, session: &SessionId) -> Result<RestoredSession, StoreError> {
    StateLayout::validate_session_id(session)?;
    let pending = ProjectionWal::pending_at(&self.layout.wal_path(session))?;
    if !pending.is_empty() {
      return Err(StoreError::Invalid(format!(
        "session {session} has an incomplete trace/session projection; reopen it to recover before continuing"
      )));
    }
    let semantic_records = SessionLog::read(&self.layout.session_path(session))?;
    let mut restored =
      session_log::restore_from_report(&self.layout.session_path(session), &semantic_records)?;
    if restored.header.session_id != *session {
      return Err(StoreError::Invalid(format!(
        "session path {} contains header for {}; resume requires recovery",
        session, restored.header.session_id
      )));
    }
    if restored.malformed_records > 0 {
      return Err(StoreError::Invalid(format!(
        "session {session} contains {} malformed semantic record(s); continuation is unsafe",
        restored.malformed_records
      )));
    }
    let trace_path = self.layout.trace_path(session);
    let trace = match TraceJournal::read(&trace_path) {
      Ok(trace) => trace,
      Err(StoreError::Missing(_)) => crate::jsonl::ReadReport {
        items: Vec::new(),
        malformed: 0,
        first_malformed_line: None,
      },
      Err(error) => return Err(error),
    };
    if trace.malformed > 0 {
      return Err(StoreError::Invalid(format!(
        "session {session} trace contains {} malformed line(s); tool recovery cannot be trusted",
        trace.malformed
      )));
    }
    if restored.checkpoint.is_some() {
      let checkpoint_id = semantic_records.items.iter().rev().find_map(|record| {
        matches!(record, SessionRecord::CheckpointBarrier(_)).then(|| match record {
          SessionRecord::CheckpointBarrier(barrier) => barrier.checkpoint_id.clone(),
          _ => unreachable!("checkpoint barrier predicate only matches its variant"),
        })
      });
      let Some(checkpoint_id) = checkpoint_id else {
        return Err(StoreError::Invalid(format!(
          "session {session} has a checkpoint capsule without a barrier; resume requires recovery"
        )));
      };
      let checkpoint_events: Vec<EventSeq> = trace
        .items
        .iter()
        .filter_map(|entry| match &entry.envelope.event {
          AgentEvent::CheckpointCreated(created) if created.checkpoint_id == checkpoint_id => {
            entry.envelope.meta.seq
          }
          _ => None,
        })
        .collect();
      if checkpoint_events.len() > 1 {
        return Err(StoreError::Invalid(format!(
          "session {session} has duplicate canonical checkpoint events for {checkpoint_id}; resume requires recovery"
        )));
      }
      // Legacy barriers (context epoch zero) predate the canonical checkpoint
      // event coordinate and retain the semantic predecessor as their floor.
      // New epoch-bearing barriers use the actual CheckpointCreated sequence so
      // resumed compaction ranges begin strictly after that event.
      let checkpoint_epoch = semantic_records
        .items
        .iter()
        .rev()
        .find_map(|record| match record {
          SessionRecord::CheckpointBarrier(barrier) if barrier.checkpoint_id == checkpoint_id => {
            Some(barrier.context_epoch)
          }
          _ => None,
        });
      if checkpoint_epoch.is_some_and(|epoch| epoch > 0) {
        let Some(seq) = checkpoint_events.first().copied() else {
          return Err(StoreError::Invalid(format!(
            "session {session} checkpoint epoch {checkpoint_epoch:?} has no canonical event; resume requires recovery"
          )));
        };
        restored.checkpoint_seq = Some(seq);
      }
    }
    validate_trace_integrity(&trace.items, session)?;
    let blobs =
      BlobStore::for_session_with_compression(&self.layout, session, self.policy.compression)?;
    validate_trace_payloads(&trace.items, &blobs, session)?;
    validate_model_request_lifecycles(&trace.items, session)?;
    // Tool side effects are process state, not checkpoint-scoped history. Scan
    // the complete canonical lifecycle before hiding pre-checkpoint events, so
    // a started mutating call can never disappear behind a later capsule.
    let interrupted_tools = interrupted_tool_calls(&trace.items)?;
    // Keep the full trace for integrity and lifecycle checks. Checkpoint
    // filtering belongs only to model-visible projection reconstruction; hiding
    // older canonical events before validation would let a damaged prefix pass.
    validate_compaction_lifecycles(&trace.items, session)?;
    validate_checkpoint_lifecycles(&trace.items, session)?;
    validate_checkpoint_capsules(&self.layout, &semantic_records.items, session)?;
    validate_projection_alignment(
      &trace.items,
      &semantic_records.items,
      &blobs,
      session,
      restored.checkpoint_seq,
    )?;
    restored.interrupted_tools = interrupted_tools;
    let from_trace = trace
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
    StateLayout::validate_session_id(session).is_ok() && self.layout.session_path(session).exists()
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
      let summary =
        SessionLog::summary(&self.layout.session_path(&id), &self.layout.trace_path(&id))?;
      if summary.session_id != id {
        return Err(StoreError::Invalid(format!(
          "session path contains header for {}; listing requires recovery",
          summary.session_id
        )));
      }
      summaries.push(summary);
    }
    Ok(summaries)
  }

  /// Full summary for one session, including counts and a preview.
  pub fn details(
    &self,
    session: &SessionId,
    preview_chars: usize,
  ) -> Result<SessionSummary, StoreError> {
    StateLayout::validate_session_id(session)?;
    let summary = SessionLog::summary_report(
      &self.layout.session_path(session),
      &self.layout.trace_path(session),
      preview_chars,
    )?;
    if summary.session_id != *session {
      return Err(StoreError::Invalid(format!(
        "session path {} contains header for {}; details requires recovery",
        session, summary.session_id
      )));
    }
    Ok(summary)
  }

  pub fn blobs(&self, session: &SessionId) -> Result<BlobStore, StoreError> {
    StateLayout::validate_session_id(session)?;
    BlobStore::for_session_with_compression(&self.layout, session, self.policy.compression)
  }

  /// Total bytes held under the sessions directory.
  pub fn used_bytes(&self) -> Result<u64, StoreError> {
    self.layout.state_bytes()
  }

  pub fn remove(&self, session: &SessionId) -> Result<u64, StoreError> {
    StateLayout::validate_session_id(session)?;
    let lease = SessionLease::acquire(&self.layout.lease_path(session))?;
    let freed = self.layout.session_bytes(session)?;
    self.layout.remove_session(session)?;
    drop(lease);
    Ok(freed)
  }

  /// List all checkpoint capsules recorded for the given session.
  pub fn list_checkpoints(
    &self,
    session: &SessionId,
  ) -> Result<Vec<(CheckpointId, ContextCapsule)>, StoreError> {
    StateLayout::validate_session_id(session)?;
    let dir = self.layout.checkpoints_dir(session);
    if !dir.exists() {
      return Ok(Vec::new());
    }
    let semantic = SessionLog::read(&self.layout.session_path(session))?;
    validate_checkpoint_capsules(&self.layout, &semantic.items, session)?;
    let committed = committed_checkpoint_ids(&self.layout.session_path(session))?;
    let mut checkpoints = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
      let entry = entry?;
      let path = entry.path();
      if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
          let id = CheckpointId::from_string(stem);
          if !committed.contains(&id) {
            continue;
          }
          let bytes = std::fs::read(&path)?;
          let capsule = serde_json::from_slice::<ContextCapsule>(&bytes).map_err(|error| {
            StoreError::Invalid(format!(
              "session {session} checkpoint {id} capsule is invalid: {error}"
            ))
          })?;
          checkpoints.push((id, capsule));
        }
      }
    }
    checkpoints.sort_by_key(|a| a.0.clone());
    Ok(checkpoints)
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
  wal: ProjectionWal,
  _lease: SessionLease,
  blobs: BlobStore,
  policy: WritePolicy,
  #[cfg(test)]
  failpoint: Option<ProjectionFailpoint>,
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

  /// Checked sequence reservation for callers that can surface exhaustion.
  pub fn checked_next_seq(&self) -> Result<EventSeq, StoreError> {
    self.journal.checked_next_seq()
  }

  pub fn blobs(&self) -> &BlobStore {
    &self.blobs
  }

  pub fn policy(&self) -> &WritePolicy {
    &self.policy
  }

  #[cfg(test)]
  fn set_failpoint(&mut self, failpoint: ProjectionFailpoint) {
    self.failpoint = Some(failpoint);
  }

  #[cfg(test)]
  fn hit_failpoint(&mut self, failpoint: ProjectionFailpoint) -> Result<(), StoreError> {
    if self.failpoint == Some(failpoint) {
      self.failpoint = None;
      return Err(StoreError::Invalid(format!(
        "synthetic projection interruption at {failpoint:?}"
      )));
    }
    Ok(())
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

  /// Atomically stage the canonical event and its semantic projection through
  /// the session WAL. `hold_for_message` is used for events whose full
  /// model-visible message is supplied immediately afterward by the runtime.
  pub fn emit_transaction(
    &mut self,
    envelope: &mut EventEnvelope,
    mut projection: Option<SessionRecord>,
    hold_for_message: bool,
  ) -> Result<EventSeq, StoreError> {
    if let Some(record) = projection.as_ref() {
      validate_projection_size(&envelope.meta.event_id, record, &self.policy.redaction)?;
      validate_session_record_size(record, &self.policy.redaction)?;
    }
    let tx_id = self.wal.prepare(envelope)?;
    let seq = self.journal.append_bounded(
      envelope,
      None,
      Some(&self.blobs),
      self.policy.inline_threshold_bytes,
    )?;
    envelope.meta.seq = Some(seq);
    if let Some(record) = projection.as_mut() {
      stamp_projection_seq(record, seq);
      self.wal.set_projection(&tx_id, record)?;
      self.log.append(record)?;
      self.wal.commit(&tx_id)?;
    } else if !hold_for_message {
      self.wal.commit(&tx_id)?;
    }
    Ok(seq)
  }

  /// Emit a canonical event and its exact model-visible message as one durable
  /// transaction. The recovery payload is prepared before the WAL prepare, so a
  /// canonical event can never outlive the bytes needed to rebuild its semantic
  /// projection.
  pub fn emit_message(
    &mut self,
    envelope: &mut EventEnvelope,
    message: &Message,
  ) -> Result<EventSeq, StoreError> {
    if envelope.meta.seq.is_some() {
      let record = self.complete_message(&rupi_core::AttributedMessage {
        envelope: envelope.clone(),
        message: message.clone(),
      })?;
      return record.seq.ok_or_else(|| {
        StoreError::Invalid(format!(
          "message {} completed without a canonical sequence",
          record.event_id
        ))
      });
    }

    validate_message_envelope(envelope)?;
    // Preflight the final redacted semantic line before creating recovery
    // payloads or changing durable state. Otherwise the canonical event could
    // be committed while the projection is permanently too large to append.
    let projected = self.message_record(envelope, message, self.journal.next_seq())?;
    validate_session_record_size(&projected, &self.policy.redaction)?;
    let (recovery, cleanup_blob) = self.prepare_message_recovery(message)?;
    let tx_id = match self.wal.prepare_message(envelope, recovery) {
      Ok(tx_id) => tx_id,
      Err(error) => {
        if let Some(blob) = cleanup_blob.as_ref() {
          self.remove_unreferenced_recovery_blob(blob);
        }
        return Err(error);
      }
    };
    #[cfg(test)]
    self.hit_failpoint(ProjectionFailpoint::Prepare)?;
    let seq = self.journal.append_bounded(
      envelope,
      None,
      Some(&self.blobs),
      self.policy.inline_threshold_bytes,
    )?;
    envelope.meta.seq = Some(seq);
    #[cfg(test)]
    self.hit_failpoint(ProjectionFailpoint::CanonicalAppend)?;
    let record = self.message_record(envelope, message, seq)?;
    self.log.append(&record)?;
    #[cfg(test)]
    self.hit_failpoint(ProjectionFailpoint::SemanticAppend)?;
    self.wal.commit(&tx_id)?;
    Ok(seq)
  }

  /// Commit a held event transaction after its multi-event lifecycle has
  /// completed. This is used for compaction's start marker, whose intent must
  /// remain open until the summary and completion are durable.
  pub fn commit_projection_intent(&mut self, event_id: &EventId) -> Result<(), StoreError> {
    if self
      .wal
      .pending()?
      .iter()
      .any(|intent| intent.tx_id == *event_id)
    {
      self.wal.commit(event_id)?;
    }
    Ok(())
  }

  fn recover_message(&self, recovery: &MessageRecovery) -> Result<Message, StoreError> {
    let bytes = match recovery {
      MessageRecovery::Inline { message } => {
        return Ok((**message).clone());
      }
      MessageRecovery::Blob { blob } => self
        .blobs
        .get_relative_verified(&blob.relative_path())
        .map_err(|error| {
          StoreError::Invalid(format!(
            "message recovery blob {} is unreadable: {error}",
            blob.relative_path()
          ))
        })?,
    };
    serde_json::from_slice(&bytes).map_err(|error| {
      StoreError::Invalid(format!(
        "message recovery payload is not a valid session message: {error}"
      ))
    })
  }

  fn prepare_message_recovery(
    &self,
    message: &Message,
  ) -> Result<(MessageRecovery, Option<BlobRef>), StoreError> {
    let mut value = serde_json::to_value(message)?;
    self.policy.redaction.apply_json(&mut value);
    let sanitized: Message = serde_json::from_value(value)?;
    let encoded = serde_json::to_vec(&sanitized)?;
    // Keep the usual inline budget for the WAL payload, with a hard ceiling so
    // an unusually configured trace budget cannot make recovery metadata large.
    const MAX_INLINE_MESSAGE_BYTES: usize = 32 * 1024;
    if encoded.len()
      <= usize::try_from(self.policy.inline_threshold_bytes)
        .unwrap_or(MAX_INLINE_MESSAGE_BYTES)
        .min(MAX_INLINE_MESSAGE_BYTES)
    {
      return Ok((
        MessageRecovery::Inline {
          message: Box::new(sanitized),
        },
        None,
      ));
    }
    let planned = self
      .blobs
      .reference_for(&encoded, Some("application/json"))?;
    let existed = self.blobs.exists(&planned) && self.blobs.verify(&planned)?;
    let blob = self.blobs.put(&encoded, Some("application/json"))?;
    Ok((
      MessageRecovery::Blob { blob: blob.clone() },
      (!existed).then_some(blob),
    ))
  }

  fn remove_unreferenced_recovery_blob(&self, blob: &BlobRef) {
    let Ok(pending) = self.wal.pending() else {
      // If WAL state cannot be read, retain the blob. Recovery safety is more
      // important than reclaiming one payload whose liveness is uncertain.
      return;
    };
    let referenced = pending.iter().any(|intent| {
      matches!(
        intent.envelope.recovery.as_ref(),
        Some(MessageRecovery::Blob { blob: candidate }) if candidate == blob
      )
    });
    if !referenced {
      let _ = self.blobs.remove(blob);
    }
  }

  fn message_record(
    &self,
    envelope: &EventEnvelope,
    message: &Message,
    seq: EventSeq,
  ) -> Result<SessionRecord, StoreError> {
    validate_message_envelope(envelope)?;
    let meta = &envelope.meta;
    let turn_id = meta
      .turn_id
      .as_ref()
      .ok_or_else(|| StoreError::Invalid("a persisted message must belong to a turn".into()))?;
    let epoch = meta
      .model_epoch
      .ok_or_else(|| StoreError::Invalid("a persisted message must carry a model epoch".into()))?;
    let model = meta
      .model
      .as_ref()
      .ok_or_else(|| StoreError::Invalid("a persisted message must carry a model".into()))?;
    let external_context = match &envelope.event {
      AgentEvent::ExternalContextRetrieved(retrieved) => Some(ExternalContextRef {
        provider: retrieved.source.provider.clone(),
        resource_id: retrieved.source.resource_id.clone(),
        citation: retrieved.citation.clone(),
        provenance: retrieved.source.provenance.clone(),
        metadata: retrieved.metadata.clone(),
      }),
      _ => None,
    };
    Ok(SessionRecord::Message(SessionMessage {
      turn_id: turn_id.clone(),
      role: message.role,
      message: message.clone(),
      epoch,
      model: model.clone(),
      event_id: meta.event_id.clone(),
      seq: Some(seq),
      external_context,
    }))
  }

  /// Complete the pending event transaction with the model-visible message.
  ///
  /// A missing pending intent is retained as a compatibility fallback for
  /// callers that use the low-level `emit`/`append_message` pair directly.
  pub fn complete_message(
    &mut self,
    attributed: &rupi_core::AttributedMessage,
  ) -> Result<SessionMessage, StoreError> {
    let meta = &attributed.envelope.meta;
    let turn_id = meta
      .turn_id
      .as_ref()
      .ok_or_else(|| StoreError::Invalid("a persisted message must belong to a turn".into()))?;
    let epoch = meta
      .model_epoch
      .ok_or_else(|| StoreError::Invalid("a persisted message must carry a model epoch".into()))?;
    let model = meta
      .model
      .as_ref()
      .ok_or_else(|| StoreError::Invalid("a persisted message must carry a model".into()))?;
    let pending = self.wal.pending()?;
    let intent = pending
      .iter()
      .find(|intent| intent.envelope.event_id == meta.event_id)
      .or_else(|| {
        pending.iter().find(|intent| {
          intent.envelope.turn_id.as_ref() == Some(turn_id)
            && intent.envelope.model_epoch == Some(epoch)
            && matches!(
              &intent.envelope.kind,
              crate::projection::WalEventKind::ModelRequestCompleted { terminal: true }
            )
        })
      });
    let Some(intent) = intent else {
      return self.append_message(
        turn_id,
        &attributed.message,
        epoch,
        model,
        &attributed.envelope,
      );
    };
    let seq = meta.seq.ok_or_else(|| {
      StoreError::Invalid(
        "the event introducing a message must be emitted first, so that the session record points at a real journal position".into(),
      )
    })?;
    let external_context = match &attributed.envelope.event {
      AgentEvent::ExternalContextRetrieved(retrieved) => Some(ExternalContextRef {
        provider: retrieved.source.provider.clone(),
        resource_id: retrieved.source.resource_id.clone(),
        citation: retrieved.citation.clone(),
        provenance: retrieved.source.provenance.clone(),
        metadata: retrieved.metadata.clone(),
      }),
      _ => None,
    };
    let record = SessionRecord::Message(SessionMessage {
      turn_id: turn_id.clone(),
      role: attributed.message.role,
      message: attributed.message.clone(),
      epoch,
      model: model.clone(),
      event_id: meta.event_id.clone(),
      seq: Some(seq),
      external_context,
    });
    validate_session_record_size(&record, &self.policy.redaction)?;
    // A held message intent may have been prepared before the runtime had the
    // complete response (for compatibility with older callers). Attach the
    // exact redacted payload before the semantic append so a crash in this
    // interval is still recoverable. New runtime paths use `emit_message`,
    // which prepares this payload before the canonical append.
    if intent.envelope.recovery.is_none() {
      let (recovery, cleanup_blob) = self.prepare_message_recovery(&attributed.message)?;
      if let Err(error) = self.wal.set_recovery(&intent.tx_id, recovery) {
        if let Some(blob) = cleanup_blob.as_ref() {
          self.remove_unreferenced_recovery_blob(blob);
        }
        return Err(error);
      }
    }
    self.log.append(&record)?;
    self.wal.commit(&intent.tx_id)?;
    let SessionRecord::Message(message) = record else {
      unreachable!("message transaction always carries a message projection")
    };
    Ok(message)
  }

  /// Append a trace event together with the redacted bytes that produced it.
  ///
  /// When raw capture is disabled the bytes are dropped without an error, and no
  /// recovery pointer is recorded. When it is enabled, the provider wire payload
  /// still crosses the store's redaction boundary before it reaches the blob store.
  pub fn emit_with_payload(
    &mut self,
    envelope: &mut EventEnvelope,
    raw: &[u8],
  ) -> Result<EventSeq, StoreError> {
    if !self.policy.raw_payload.is_enabled() {
      return self.emit(envelope);
    }
    let redacted = redact_payload(&self.policy.redaction, raw)?;
    let blob = self.blobs.put(&redacted, None)?;
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
    let external_context = match &envelope.event {
      AgentEvent::ExternalContextRetrieved(retrieved) => Some(ExternalContextRef {
        provider: retrieved.source.provider.clone(),
        resource_id: retrieved.source.resource_id.clone(),
        citation: retrieved.citation.clone(),
        provenance: retrieved.source.provenance.clone(),
        metadata: retrieved.metadata.clone(),
      }),
      _ => None,
    };
    let record = SessionMessage {
      turn_id: turn_id.clone(),
      role: message.role,
      message: message.clone(),
      epoch,
      model: model.clone(),
      event_id: envelope.meta.event_id.clone(),
      seq: Some(seq),
      external_context,
    };
    self.log.append(&SessionRecord::Message(record.clone()))?;
    Ok(record)
  }

  /// Write a checkpoint capsule without publishing its semantic barrier.
  ///
  /// `StoreTrace` uses this two-phase form so the following `CheckpointCreated`
  /// trace event and its barrier share one WAL transaction. An orphan capsule
  /// after a crash is harmless; a barrier without a capsule is not.
  pub fn prepare_checkpoint(
    &mut self,
    capsule: &ContextCapsule,
  ) -> Result<SessionCheckpointRecord, StoreError> {
    let checkpoint_id = CheckpointId::new();
    let path = self.layout.checkpoint_path(self.id(), &checkpoint_id);
    if let Some(dir) = path.parent() {
      std::fs::create_dir_all(dir)?;
    }
    let mut durable_capsule = serde_json::to_value(capsule)?;
    self.policy.redaction.apply_json(&mut durable_capsule);
    let durable_capsule = serde_json::from_value::<ContextCapsule>(durable_capsule)?;
    let encoded = serde_json::to_vec_pretty(&durable_capsule)?;
    if encoded.len() as u64 > MAX_CAPSULE_BYTES {
      return Err(StoreError::Invalid(format!(
        "checkpoint capsule exceeds the {MAX_CAPSULE_BYTES}-byte bound"
      )));
    }
    let mut checkpoint_files = 0usize;
    let mut checkpoint_bytes = 0u64;
    if let Some(dir) = path.parent() {
      for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.path().extension().and_then(|ext| ext.to_str()) != Some("json") {
          continue;
        }
        checkpoint_files = checkpoint_files
          .checked_add(1)
          .ok_or_else(|| StoreError::Invalid("checkpoint file count is exhausted".into()))?;
        checkpoint_bytes = checkpoint_bytes
          .checked_add(entry.metadata()?.len())
          .ok_or_else(|| StoreError::Invalid("checkpoint bytes are exhausted".into()))?;
      }
    }
    if checkpoint_files >= MAX_CHECKPOINT_COUNT {
      return Err(StoreError::Invalid(format!(
        "session {} contains more than {MAX_CHECKPOINT_COUNT} checkpoint files",
        self.id()
      )));
    }
    if checkpoint_bytes
      .checked_add(encoded.len() as u64)
      .is_none_or(|total| total > MAX_CHECKPOINT_TOTAL_BYTES)
    {
      return Err(StoreError::Invalid(format!(
        "session {} checkpoint capsules exceed the {MAX_CHECKPOINT_TOTAL_BYTES}-byte aggregate bound",
        self.id()
      )));
    }
    let temporary = path.with_extension("json.tmp");
    {
      let mut file = std::fs::File::create(&temporary)?;
      use std::io::Write;
      file.write_all(&encoded)?;
      file.sync_all()?;
    }
    std::fs::rename(&temporary, &path)?;
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
      if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
      }
    }

    Ok(SessionCheckpointRecord {
      capsule_path: format!("checkpoints/{checkpoint_id}.json"),
      checkpoint_id,
      capsule_version: durable_capsule.version,
      context_epoch: 0,
      capsule: durable_capsule,
    })
  }

  /// Publish a checkpoint barrier after its `CheckpointCreated` event.
  pub fn append_checkpoint_barrier(
    &mut self,
    record: &SessionCheckpointRecord,
  ) -> Result<(), StoreError> {
    self
      .log
      .append(&SessionRecord::CheckpointBarrier(record.clone()))
  }

  /// Materialize a checkpoint barrier whose WAL projection was interrupted.
  ///
  /// The barrier is safe to finish before the L3 completion is synthesized: the
  /// capsule file and canonical `CheckpointCreated` event are already durable,
  /// and the barrier is what proves the file belongs to this session.
  fn recover_pending_checkpoint_barriers(
    &mut self,
    trace: &[rupi_core::TraceEntry],
    records: &mut Vec<SessionRecord>,
  ) -> Result<(), StoreError> {
    for intent in self.wal.pending()? {
      let Some(trace_entry) = trace
        .iter()
        .find(|entry| entry.envelope.meta.event_id == intent.envelope.event_id)
      else {
        continue;
      };
      if !matches!(trace_entry.envelope.event, AgentEvent::CheckpointCreated(_)) {
        continue;
      }
      let record = match intent.record {
        Some(record) => record,
        None => {
          recover_projection_record(trace_entry, trace, records, self, None)?.ok_or_else(|| {
            StoreError::Invalid(format!(
              "session {} checkpoint event has no recoverable barrier projection",
              self.id()
            ))
          })?
        }
      };
      let SessionRecord::CheckpointBarrier(barrier) = record else {
        return Err(StoreError::Invalid(format!(
          "session {} checkpoint event has a non-barrier projection",
          self.id()
        )));
      };
      if !records.iter().any(|candidate| {
        matches!(
          candidate,
          SessionRecord::CheckpointBarrier(existing)
            if existing.checkpoint_id == barrier.checkpoint_id
        )
      }) {
        self
          .log
          .append(&SessionRecord::CheckpointBarrier(barrier.clone()))?;
        records.push(SessionRecord::CheckpointBarrier(barrier));
      }
      self.wal.commit(&intent.tx_id)?;
    }
    Ok(())
  }

  /// Reconcile uncommitted trace/session projection intents before a resumed
  /// runtime is allowed to issue a provider request.
  fn recover_projection(&mut self) -> Result<(), StoreError> {
    // Recovery may need to repair a lifecycle even when its final WAL intent
    // was already committed: a checkpoint barrier is durable before its L3
    // completion, and an interrupted L1/L2 summary can have committed its own
    // message projection before the held start intent. Inspect canonical state
    // before deciding that there is no WAL work left.
    let trace_report = TraceJournal::read(self.trace_path())?;
    if trace_report.malformed > 0 {
      return Err(StoreError::Invalid(format!(
        "session {} trace contains {} malformed line(s); projection recovery is unsafe",
        self.id(),
        trace_report.malformed
      )));
    }
    validate_trace_integrity(&trace_report.items, self.id())?;
    inspect_model_request_lifecycles(&trace_report.items, self.id())?;
    let mut records = SessionLog::read(self.path())?.items;
    self.recover_pending_checkpoint_barriers(&trace_report.items, &mut records)?;
    records = SessionLog::read(self.path())?.items;
    recover_incomplete_compactions(self, &trace_report.items, &mut records)?;

    let trace_report = TraceJournal::read(self.trace_path())?;
    if trace_report.malformed > 0 {
      return Err(StoreError::Invalid(format!(
        "session {} trace contains {} malformed line(s); projection recovery is unsafe",
        self.id(),
        trace_report.malformed
      )));
    }
    validate_trace_integrity(&trace_report.items, self.id())?;
    records = SessionLog::read(self.path())?.items;
    recover_incomplete_checkpoints(self, &trace_report.items, &records)?;

    let trace_report = TraceJournal::read(self.trace_path())?;
    if trace_report.malformed > 0 {
      return Err(StoreError::Invalid(format!(
        "session {} trace contains {} malformed line(s); projection recovery is unsafe",
        self.id(),
        trace_report.malformed
      )));
    }
    validate_trace_integrity(&trace_report.items, self.id())?;
    validate_compaction_lifecycles(&trace_report.items, self.id())?;
    validate_checkpoint_lifecycles(&trace_report.items, self.id())?;
    let trace = trace_report.items;
    records = SessionLog::read(self.path())?.items;
    let aborted_events = aborted_compaction_event_ids(&trace);
    let mut pending = self.wal.pending()?;
    if pending.is_empty() {
      return Ok(());
    }
    pending.sort_by_key(|intent| {
      trace
        .iter()
        .find(|entry| entry.envelope.meta.event_id == intent.envelope.event_id)
        .and_then(|entry| entry.envelope.meta.seq)
        .unwrap_or(EventSeq(u64::MAX))
    });
    for intent in pending {
      let trace_entry = trace
        .iter()
        .find(|entry| entry.envelope.meta.event_id == intent.envelope.event_id);
      let Some(trace_entry) = trace_entry else {
        // The prepare reached the WAL but the canonical append did not. There
        // is no fact to project; committing the abandoned intent is safe. A
        // projection without its trace counterpart, however, is corruption.
        if intent
          .record
          .as_ref()
          .is_some_and(|record| has_projection_for(record, &records))
        {
          return Err(StoreError::Invalid(format!(
            "session {} has a semantic projection without its canonical event {}",
            self.id(),
            intent.envelope.event_id
          )));
        }
        self.wal.commit(&intent.tx_id)?;
        continue;
      };

      // A staged summary/epoch from an aborted L1/L2 compaction is canonical
      // evidence but not model-visible state. Its message projection may have
      // been interrupted at any point, so close the intent without attempting
      // to reconstruct or append the discarded summary.
      if aborted_events.contains(&trace_entry.envelope.meta.event_id) {
        self.wal.commit(&intent.tx_id)?;
        continue;
      }

      if matches!(
        &intent.envelope.kind,
        crate::projection::WalEventKind::ContextCompactionStarted
      ) {
        let start_seq = trace_entry.envelope.meta.seq;
        let completed = trace.iter().any(|candidate| {
          let Some(candidate_seq) = candidate.envelope.meta.seq else {
            return false;
          };
          candidate_seq > start_seq.unwrap_or(EventSeq(0))
            && candidate.envelope.meta.turn_id == trace_entry.envelope.meta.turn_id
            && matches!(
              &candidate.envelope.event,
              AgentEvent::ContextCompactionCompleted(completed)
                if completed.level != rupi_core::ContextLevel::L3Checkpoint
            )
        });
        let aborted = trace
          .iter()
          .any(|entry| is_compaction_abort(entry, &trace_entry.envelope.meta.event_id));
        if !completed && !aborted {
          return Err(StoreError::Invalid(format!(
            "session {} has an incomplete context-compaction lifecycle; resume requires recovery",
            self.id()
          )));
        }
        self.wal.commit(&intent.tx_id)?;
        continue;
      }

      let recovery = intent.envelope.recovery.clone();
      let record = match intent.record {
        Some(record) => Some(record),
        None => {
          // A message transaction intentionally keeps only a compact prepare
          // in the WAL. If its session append won the crash race, committing
          // that intent is safe and avoids reconstructing a payload that may
          // be blob-backed in the canonical trace.
          if records.iter().any(|record| match record {
            SessionRecord::Message(message) => message.event_id == intent.envelope.event_id,
            _ => false,
          }) {
            self.wal.commit(&intent.tx_id)?;
            continue;
          }
          recover_projection_record(trace_entry, &trace, &records, self, recovery.as_ref())?
        }
      };
      let Some(record) = record else {
        self.wal.commit(&intent.tx_id)?;
        continue;
      };
      let already_projected = has_projection_for(&record, &records);
      if !already_projected {
        self.log.append(&record)?;
        records.push(record.clone());
      }
      // Once the semantic append is durable, the prepare can commit directly.
      // Re-attaching a reconstructed payload to the WAL would duplicate large
      // messages or checkpoint capsules and could recreate an oversized-WAL
      // deadlock on the next restart.
      self.wal.commit(&intent.tx_id)?;
    }
    Ok(())
  }

  /// Normalize a model request that was durably started but never completed.
  ///
  /// A provider may have received the request, so the request is never retried
  /// automatically. Closing it as `abandoned` preserves the canonical deltas
  /// while keeping them out of the resumed model-visible message projection.
  fn recover_abandoned_model_requests(&mut self) -> Result<(), StoreError> {
    let report = TraceJournal::read(self.trace_path())?;
    if report.malformed > 0 {
      return Err(StoreError::Invalid(format!(
        "session {} trace contains {} malformed record(s); model recovery is unsafe",
        self.id(),
        report.malformed
      )));
    }
    let starts = inspect_model_request_lifecycles(&report.items, self.id())?;
    for start in starts {
      let started = match &start.envelope.event {
        AgentEvent::ModelRequestStarted(started) => started,
        _ => unreachable!("lifecycle inspection returns only open request starts"),
      };
      let mut completion_meta =
        EventMeta::new(self.id().clone(), start.envelope.meta.trace_id.clone());
      completion_meta.turn_id = start.envelope.meta.turn_id.clone();
      completion_meta.model_epoch = Some(started.epoch);
      completion_meta.model = Some(started.model.clone());
      completion_meta.parent_event_id = Some(start.envelope.meta.event_id.clone());
      let mut completion = EventEnvelope::new(
        completion_meta,
        AgentEvent::ModelRequestCompleted(ModelRequestCompleted {
          epoch: started.epoch,
          model: started.model.clone(),
          finish_reason: Some("abandoned".into()),
          input_tokens: None,
          logical_prompt_tokens: None,
          cache_read_tokens: None,
          cache_write_tokens: None,
          output_tokens: None,
          provider_total_tokens: None,
          duration_ms: 0,
          tool_calls: 0,
          reasoning_provenance: None,
          first_delta_ms: None,
        }),
      );
      self.emit(&mut completion)?;

      let mut diagnostic_meta =
        EventMeta::new(self.id().clone(), start.envelope.meta.trace_id.clone());
      diagnostic_meta.turn_id = start.envelope.meta.turn_id.clone();
      diagnostic_meta.model_epoch = Some(started.epoch);
      diagnostic_meta.model = Some(started.model.clone());
      diagnostic_meta.parent_event_id = Some(completion.meta.event_id.clone());
      let mut diagnostic = EventEnvelope::new(
        diagnostic_meta,
        AgentEvent::Diagnostic(Diagnostic {
          level: DiagnosticLevel::Warn,
          message: format!(
            "model request {} was interrupted before completion; partial output remains canonical and was not restored into model context",
            start.envelope.meta.event_id
          ),
        }),
      );
      self.emit(&mut diagnostic)?;
    }
    Ok(())
  }

  /// Close assistant tool calls that were projected before execution crossed
  /// the `ToolRequested` boundary. The assistant message is durable evidence of
  /// intent, while the ordering contract proves that no tool code could have
  /// run. The synthesized request is parented to that assistant event so the
  /// recovery-generated lifecycle remains explicit in the canonical trace.
  fn recover_unrequested_assistant_calls(&mut self) -> Result<(), StoreError> {
    let trace_report = TraceJournal::read(self.trace_path())?;
    if trace_report.malformed > 0 {
      return Err(StoreError::Invalid(format!(
        "session {} trace contains {} malformed record(s); assistant-call recovery is unsafe",
        self.id(),
        trace_report.malformed
      )));
    }
    let semantic = SessionLog::read(self.path())?;
    let mut missing = Vec::new();
    for record in semantic.items.iter().filter_map(|record| match record {
      SessionRecord::Message(message) if message.role == Role::Assistant => Some(message),
      _ => None,
    }) {
      // Provider call ids are scoped to one assistant response. A provider may
      // reuse an id on a later turn/model round, so never make this set global
      // to the whole session.
      let mut assistant_call_ids = BTreeSet::new();
      let assistant = trace_report
        .items
        .iter()
        .find(|entry| entry.envelope.meta.event_id == record.event_id)
        .ok_or_else(|| {
          StoreError::Invalid(format!(
            "session {} assistant message {} has no canonical event; resume requires recovery",
            self.id(),
            record.event_id
          ))
        })?;
      for call in record.message.tool_calls() {
        if !assistant_call_ids.insert(call.id.clone()) {
          return Err(StoreError::Invalid(format!(
            "session {} assistant tool call {} appears more than once; resume requires recovery",
            self.id(),
            call.id
          )));
        }
        let mut requests = Vec::new();
        for entry in &trace_report.items {
          let event = restore_externalized_event(entry, self)?;
          if let AgentEvent::ToolRequested(requested) = event
            && requested.call_id == call.id
            && (entry.envelope.meta.parent_event_id.as_ref()
              == Some(&assistant.envelope.meta.event_id)
              || (entry.envelope.meta.parent_event_id.is_none()
                && entry.envelope.meta.turn_id == Some(record.turn_id.clone())
                && entry
                  .envelope
                  .meta
                  .seq
                  .zip(assistant.envelope.meta.seq)
                  .is_some_and(|(request_seq, assistant_seq)| request_seq > assistant_seq)))
          {
            requests.push((entry, requested));
          }
        }
        if requests.len() > 1 {
          return Err(StoreError::Invalid(format!(
            "session {} assistant tool call {} has duplicate canonical requests; resume requires recovery",
            self.id(),
            call.id
          )));
        }
        // Pi imports may omit model metadata on tool events; an explicit
        // mismatch remains corruption, while absent metadata stays compatible.
        let exact = requests.first().is_some_and(|(entry, requested)| {
          requested.name == call.name
            && requested.arguments == call.arguments
            && entry.envelope.meta.turn_id == Some(record.turn_id.clone())
            && entry
              .envelope
              .meta
              .model_epoch
              .is_none_or(|epoch| epoch == record.epoch)
            && entry
              .envelope
              .meta
              .model
              .as_ref()
              .is_none_or(|model| model == &record.model)
            && entry
              .envelope
              .meta
              .seq
              .zip(assistant.envelope.meta.seq)
              .is_some_and(|(request_seq, assistant_seq)| request_seq > assistant_seq)
        });
        if requests.len() == 1 && !exact {
          return Err(StoreError::Invalid(format!(
            "session {} assistant tool call {} disagrees with its canonical request; resume requires recovery",
            self.id(),
            call.id
          )));
        }
        if !exact {
          missing.push((
            record.clone(),
            call.clone(),
            assistant.envelope.meta.trace_id.clone(),
          ));
        }
      }
    }

    for (assistant, call, trace_id) in missing {
      let details = "not executed: process stopped before execution boundary".to_string();
      let mut request_meta = EventMeta::new(self.id().clone(), trace_id.clone());
      request_meta.turn_id = Some(assistant.turn_id.clone());
      request_meta.model_epoch = Some(assistant.epoch);
      request_meta.model = Some(assistant.model.clone());
      request_meta.tool_call_id = Some(call.id.clone());
      request_meta.parent_event_id = Some(assistant.event_id.clone());
      let mut requested = EventEnvelope::new(
        request_meta,
        AgentEvent::ToolRequested(rupi_core::ToolRequested {
          call_id: call.id.clone(),
          name: call.name.clone(),
          arguments: call.arguments.clone(),
          // Recovery is about proving non-execution, not assigning a registry
          // risk class. Keep the conservative value for later inspection.
          read_only: false,
        }),
      );
      self.emit(&mut requested)?;

      let mut failed_meta = EventMeta::new(self.id().clone(), trace_id.clone());
      failed_meta.turn_id = Some(assistant.turn_id.clone());
      failed_meta.model_epoch = Some(assistant.epoch);
      failed_meta.model = Some(assistant.model.clone());
      failed_meta.tool_call_id = Some(call.id.clone());
      failed_meta.parent_event_id = Some(requested.meta.event_id.clone());
      let mut failed = EventEnvelope::new(
        failed_meta,
        AgentEvent::ToolFailed(rupi_core::ToolFailed {
          call_id: call.id.clone(),
          name: call.name.clone(),
          message: details.clone(),
          duration_ms: 0,
          status: None,
        }),
      );
      let message = Message::new(
        Role::Tool,
        vec![ContentBlock::ToolResult(ToolResultBlock {
          id: call.id,
          name: call.name,
          state: ToolExecutionState::Failed,
          text: details,
          is_error: true,
          reduced: false,
        })],
      );
      self.emit_message(&mut failed, &message)?;
    }
    Ok(())
  }

  /// Close tool requests that were recorded before execution crossed the
  /// `ToolStarted` boundary. This is provably safe: no tool code could have
  /// run, so a protocol-completing failed result is preferable to bricking the
  /// session or replaying an operation blindly.
  fn recover_unstarted_tool_requests(&mut self) -> Result<(), StoreError> {
    let report = TraceJournal::read(self.trace_path())?;
    if report.malformed > 0 {
      return Err(StoreError::Invalid(format!(
        "session {} trace contains {} malformed record(s); tool recovery is unsafe",
        self.id(),
        report.malformed
      )));
    }
    for entry in unstarted_tool_requests(&report.items, self.id())? {
      let requested = match &entry.envelope.event {
        AgentEvent::ToolRequested(requested) => requested,
        _ => unreachable!("tool inspection returns only requests without starts"),
      };
      let details = format!(
        "tool request '{}' was interrupted before execution; no side effect was observed",
        requested.name
      );
      let mut meta = EventMeta::new(self.id().clone(), entry.envelope.meta.trace_id.clone());
      meta.turn_id = entry.envelope.meta.turn_id.clone();
      meta.model_epoch = entry.envelope.meta.model_epoch;
      meta.model = entry.envelope.meta.model.clone();
      meta.tool_call_id = Some(requested.call_id.clone());
      meta.parent_event_id = Some(entry.envelope.meta.event_id.clone());
      let mut envelope = EventEnvelope::new(
        meta,
        AgentEvent::ToolFailed(ToolFailed {
          call_id: requested.call_id.clone(),
          name: requested.name.clone(),
          message: details.clone(),
          duration_ms: 0,
          status: None,
        }),
      );
      let message = Message::new(
        Role::Tool,
        vec![ContentBlock::ToolResult(ToolResultBlock {
          id: requested.call_id.clone(),
          name: requested.name.clone(),
          state: ToolExecutionState::Failed,
          text: details,
          is_error: true,
          reduced: false,
        })],
      );
      self.emit_message(&mut envelope, &message)?;
    }
    Ok(())
  }

  /// Write a checkpoint capsule and its barrier for low-level callers that do
  /// not have a trace transaction to coordinate.
  pub fn checkpoint(
    &mut self,
    capsule: &ContextCapsule,
  ) -> Result<SessionCheckpointRecord, StoreError> {
    let record = self.prepare_checkpoint(capsule)?;
    self.append_checkpoint_barrier(&record)?;
    Ok(record)
  }

  /// List all checkpoint capsules for this session.
  pub fn list_checkpoints(&self) -> Result<Vec<(CheckpointId, ContextCapsule)>, StoreError> {
    let dir = self.layout.checkpoints_dir(self.id());
    if !dir.exists() {
      return Ok(Vec::new());
    }
    let semantic = SessionLog::read(self.path())?;
    validate_checkpoint_capsules(&self.layout, &semantic.items, self.id())?;
    let committed = committed_checkpoint_ids(self.path())?;
    let mut checkpoints = Vec::new();
    let mut files = 0usize;
    let mut total_bytes = 0u64;
    for entry in std::fs::read_dir(&dir)? {
      let entry = entry?;
      let path = entry.path();
      if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
        files = files.checked_add(1).ok_or_else(|| {
          StoreError::Invalid(format!(
            "session {} checkpoint file count is exhausted",
            self.id()
          ))
        })?;
        if files > MAX_CHECKPOINT_COUNT {
          return Err(StoreError::Invalid(format!(
            "session {} contains more than {MAX_CHECKPOINT_COUNT} checkpoint files",
            self.id()
          )));
        }
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
          let id = CheckpointId::from_string(stem);
          if !committed.contains(&id) {
            continue;
          }
          let size = std::fs::metadata(&path)?.len();
          total_bytes = total_bytes.checked_add(size).ok_or_else(|| {
            StoreError::Invalid(format!(
              "session {} checkpoint bytes are exhausted",
              self.id()
            ))
          })?;
          if total_bytes > MAX_CHECKPOINT_TOTAL_BYTES {
            return Err(StoreError::Invalid(format!(
              "session {} checkpoint capsules exceed the {MAX_CHECKPOINT_TOTAL_BYTES}-byte aggregate bound",
              self.id()
            )));
          }
          let capsule = read_checkpoint_capsule(&path, self.id(), &id)?;
          checkpoints.push((id, capsule));
        }
      }
    }
    checkpoints.sort_by_key(|a| a.0.clone());
    Ok(checkpoints)
  }

  /// Store a payload, redacting it at this boundary before choosing inline or
  /// blob storage by the configured threshold.
  pub fn put_payload(&mut self, bytes: &[u8]) -> Result<Payload, StoreError> {
    let redacted = redact_payload(&self.policy.redaction, bytes)?;
    if (redacted.len() as u64) < self.policy.inline_threshold_bytes {
      return Ok(Payload::Inline(
        String::from_utf8_lossy(&redacted).into_owned(),
      ));
    }
    Ok(Payload::Blob(self.blobs.put(&redacted, None)?))
  }

  /// Store recovery bytes after applying the configured durable redaction policy.
  ///
  /// Unlike [`Self::put_payload`], this always returns a blob because runtime
  /// reduction events need a stable recovery reference even for a small payload.
  pub fn put_recovery_blob(&self, bytes: &[u8]) -> Result<BlobRef, StoreError> {
    let redacted = redact_payload(&self.policy.redaction, bytes)?;
    self.blobs.put(&redacted, None)
  }

  /// Flush both logs.
  ///
  /// Streaming-only progress uses this instead of forcing a sync for every
  /// delta; state-changing events are already durable.
  pub fn flush(&mut self) -> Result<(), StoreError> {
    self.commit_empty_completion_intents()?;
    let pending = self.wal.pending()?;
    if !pending.is_empty() {
      return Err(StoreError::Invalid(format!(
        "session {} has an incomplete projection transaction; finish requires recovery",
        self.id()
      )));
    }
    self.journal.flush()?;
    self.log.flush()
  }

  /// Successful model requests with no visible assistant/tool block have no
  /// semantic message to append. Their WAL intent still must be closed before a
  /// clean session can be reopened; an interrupted request with visible content
  /// remains pending and is recovered (or refused) on the next resume.
  fn commit_empty_completion_intents(&mut self) -> Result<(), StoreError> {
    let pending = self.wal.pending()?;
    if pending.is_empty() {
      return Ok(());
    }
    let trace_report = TraceJournal::read(self.trace_path())?;
    if trace_report.malformed > 0 {
      return Err(StoreError::Invalid(format!(
        "session {} trace contains {} malformed record(s); completion intent cannot be classified",
        self.id(),
        trace_report.malformed
      )));
    }
    for intent in pending {
      if !matches!(
        &intent.envelope.kind,
        crate::projection::WalEventKind::ModelRequestCompleted { terminal: true }
      ) {
        continue;
      }
      let Some(trace_entry) = trace_report
        .items
        .iter()
        .find(|entry| entry.envelope.meta.event_id == intent.envelope.event_id)
      else {
        continue;
      };
      if recover_projection_record(trace_entry, &trace_report.items, &[], self, None)?.is_none() {
        self.wal.commit(&intent.tx_id)?;
      }
    }
    Ok(())
  }

  /// Finish with the session: flush and release appenders.
  pub fn finish(mut self) -> Result<(), StoreError> {
    self.flush()
  }
}

const COMPACTION_ABORT_PREFIX: &str = "recovered aborted context compaction ";

fn compaction_abort_message(start: &EventId) -> String {
  format!("{COMPACTION_ABORT_PREFIX}{start}; staged summary was not published")
}

fn is_compaction_abort(entry: &rupi_core::TraceEntry, start: &EventId) -> bool {
  entry.envelope.meta.parent_event_id.as_ref() == Some(start)
    && matches!(
      &entry.envelope.event,
      AgentEvent::Diagnostic(diagnostic)
        if diagnostic.message == compaction_abort_message(start)
    )
}

fn find_incomplete_compactions(
  entries: &[rupi_core::TraceEntry],
  session: &SessionId,
) -> Result<Vec<(rupi_core::TraceEntry, Option<EventId>)>, StoreError> {
  let mut open: Option<usize> = None;
  let mut incomplete = Vec::new();
  for (index, entry) in entries.iter().enumerate() {
    match &entry.envelope.event {
      AgentEvent::ContextCompactionStarted(started) => {
        if !matches!(
          started.level,
          rupi_core::ContextLevel::L1Ordinary | rupi_core::ContextLevel::L2Phase
        ) || open.is_some()
        {
          return Err(StoreError::Invalid(format!(
            "session {session} has an ambiguous context-compaction lifecycle; resume requires recovery"
          )));
        }
        open = Some(index);
      }
      AgentEvent::ContextCompactionCompleted(completed)
        if completed.level != rupi_core::ContextLevel::L3Checkpoint =>
      {
        let Some(start_index) = open.take() else {
          return Err(StoreError::Invalid(format!(
            "session {session} has a compaction completion without a matching start; resume requires recovery"
          )));
        };
        let AgentEvent::ContextCompactionStarted(started) = &entries[start_index].envelope.event
        else {
          unreachable!("open compaction always points to a start event")
        };
        if started.level != completed.level
          || entries[start_index].envelope.meta.turn_id != entry.envelope.meta.turn_id
        {
          return Err(StoreError::Invalid(format!(
            "session {session} compaction completion does not match its start; resume requires recovery"
          )));
        }
      }
      AgentEvent::Diagnostic(_) if open.is_some() => {
        let start_index = open.expect("checked above");
        let start_id = &entries[start_index].envelope.meta.event_id;
        if is_compaction_abort(entry, start_id) {
          open = None;
        }
      }
      _ => {}
    }
  }

  if let Some(start_index) = open {
    let start = entries[start_index].clone();
    let summary_ids: Vec<EventId> = entries
      .iter()
      .skip(start_index + 1)
      .filter(|entry| {
        entry.envelope.meta.turn_id == start.envelope.meta.turn_id
          && matches!(entry.envelope.event, AgentEvent::ContextSummary)
      })
      .map(|entry| entry.envelope.meta.event_id.clone())
      .collect();
    if summary_ids.len() > 1 {
      return Err(StoreError::Invalid(format!(
        "session {session} has multiple staged compaction summaries; resume requires recovery"
      )));
    }
    for entry in entries.iter().skip(start_index + 1) {
      if entry.envelope.meta.turn_id != start.envelope.meta.turn_id {
        continue;
      }
      if let AgentEvent::ContextCompactionEpoch(epoch) = &entry.envelope.event {
        if epoch.context_epoch == 0
          || epoch.replaces_from.0 == 0
          || epoch.replaces_from > epoch.replaces_through
        {
          return Err(StoreError::Invalid(format!(
            "session {session} has an invalid staged compaction epoch; resume requires recovery"
          )));
        }
      }
    }
    incomplete.push((start, summary_ids.into_iter().next()));
  }
  Ok(incomplete)
}

fn find_aborted_compactions(
  entries: &[rupi_core::TraceEntry],
  session: &SessionId,
) -> Result<Vec<(rupi_core::TraceEntry, Option<EventId>)>, StoreError> {
  let mut aborted = Vec::new();
  for (index, entry) in entries.iter().enumerate() {
    let AgentEvent::Diagnostic(diagnostic) = &entry.envelope.event else {
      continue;
    };
    let Some(start_id) = entry.envelope.meta.parent_event_id.as_ref() else {
      continue;
    };
    if diagnostic.message != compaction_abort_message(start_id) {
      continue;
    }
    let Some(start_index) = entries[..index].iter().rposition(|candidate| {
      candidate.envelope.meta.event_id == *start_id
        && matches!(
          candidate.envelope.event,
          AgentEvent::ContextCompactionStarted(_)
        )
    }) else {
      return Err(StoreError::Invalid(format!(
        "session {session} has an abort marker without a compaction start; resume requires recovery"
      )));
    };
    let AgentEvent::ContextCompactionStarted(started) = &entries[start_index].envelope.event else {
      unreachable!("aborted compaction parent search only returns starts")
    };
    if !matches!(
      started.level,
      rupi_core::ContextLevel::L1Ordinary | rupi_core::ContextLevel::L2Phase
    ) {
      return Err(StoreError::Invalid(format!(
        "session {session} has an abort marker for a non-abortable compaction; resume requires recovery"
      )));
    }
    if entries
      .iter()
      .skip(start_index + 1)
      .take(index.saturating_sub(start_index + 1))
      .any(|candidate| {
        matches!(
          candidate.envelope.event,
          AgentEvent::ContextCompactionCompleted(ref completed)
            if completed.level != rupi_core::ContextLevel::L3Checkpoint
        )
      })
    {
      return Err(StoreError::Invalid(format!(
        "session {session} has a compaction abort after completion; resume requires recovery"
      )));
    }
    let summary_ids: Vec<EventId> = entries
      .iter()
      .skip(start_index + 1)
      .take(index.saturating_sub(start_index + 1))
      .filter(|candidate| {
        candidate.envelope.meta.turn_id == entries[start_index].envelope.meta.turn_id
          && matches!(candidate.envelope.event, AgentEvent::ContextSummary)
      })
      .map(|candidate| candidate.envelope.meta.event_id.clone())
      .collect();
    if summary_ids.len() > 1 {
      return Err(StoreError::Invalid(format!(
        "session {session} has multiple staged compaction summaries; resume requires recovery"
      )));
    }
    aborted.push((entries[start_index].clone(), summary_ids.into_iter().next()));
  }
  Ok(aborted)
}

fn aborted_compaction_event_ids(entries: &[rupi_core::TraceEntry]) -> BTreeSet<EventId> {
  let mut open: Option<usize> = None;
  let mut ignored = BTreeSet::new();
  for (index, entry) in entries.iter().enumerate() {
    match &entry.envelope.event {
      AgentEvent::ContextCompactionStarted(started)
        if matches!(
          started.level,
          rupi_core::ContextLevel::L1Ordinary | rupi_core::ContextLevel::L2Phase
        ) =>
      {
        open = Some(index)
      }
      AgentEvent::Diagnostic(_) if open.is_some() => {
        let start_index = open.expect("checked above");
        let start_id = &entries[start_index].envelope.meta.event_id;
        if is_compaction_abort(entry, start_id) {
          for staged in entries
            .iter()
            .skip(start_index + 1)
            .take(index - start_index - 1)
          {
            if matches!(
              staged.envelope.event,
              AgentEvent::ContextSummary | AgentEvent::ContextCompactionEpoch(_)
            ) {
              ignored.insert(staged.envelope.meta.event_id.clone());
            }
          }
          open = None;
        }
      }
      AgentEvent::ContextCompactionCompleted(completed)
        if completed.level != rupi_core::ContextLevel::L3Checkpoint =>
      {
        open = None
      }
      _ => {}
    }
  }
  ignored
}

#[derive(Debug, Clone)]
struct IncompleteCheckpoint {
  entry: rupi_core::TraceEntry,
  created: rupi_core::CheckpointCreated,
}

fn find_incomplete_checkpoints(
  entries: &[rupi_core::TraceEntry],
  session: &SessionId,
) -> Result<Vec<IncompleteCheckpoint>, StoreError> {
  let mut pending: Option<IncompleteCheckpoint> = None;
  let mut ids = BTreeSet::new();
  let mut epochs = BTreeSet::new();
  for entry in entries {
    match &entry.envelope.event {
      AgentEvent::CheckpointCreated(created) if created.context_epoch > 0 => {
        if !ids.insert(created.checkpoint_id.clone())
          || !epochs.insert(created.context_epoch)
          || pending.is_some()
        {
          return Err(StoreError::Invalid(format!(
            "session {session} has overlapping or duplicate checkpoint lifecycles; resume requires recovery"
          )));
        }
        pending = Some(IncompleteCheckpoint {
          entry: entry.clone(),
          created: created.clone(),
        });
      }
      AgentEvent::CheckpointCreated(created) => {
        if !ids.insert(created.checkpoint_id.clone()) {
          return Err(StoreError::Invalid(format!(
            "session {session} repeats checkpoint {}; resume requires recovery",
            created.checkpoint_id
          )));
        }
      }
      AgentEvent::ContextCompactionCompleted(completed)
        if completed.level == rupi_core::ContextLevel::L3Checkpoint
          && completed.context_epoch > 0 =>
      {
        let Some(boundary) = pending.take() else {
          return Err(StoreError::Invalid(format!(
            "session {session} checkpoint completion has no preceding checkpoint; resume requires recovery"
          )));
        };
        if boundary.created.context_epoch != completed.context_epoch
          || boundary.entry.envelope.meta.turn_id != entry.envelope.meta.turn_id
          || u64::from(completed.removed_messages) != boundary.created.summarized_events
        {
          return Err(StoreError::Invalid(format!(
            "session {session} checkpoint completion does not match its boundary; resume requires recovery"
          )));
        }
      }
      _ => {}
    }
  }
  Ok(pending.into_iter().collect())
}

fn recover_incomplete_compactions(
  session: &mut Session,
  trace: &[rupi_core::TraceEntry],
  records: &mut Vec<SessionRecord>,
) -> Result<(), StoreError> {
  let mut candidates = find_incomplete_compactions(trace, session.id())?;
  let mut candidate_ids = candidates
    .iter()
    .map(|(start, _)| start.envelope.meta.event_id.clone())
    .collect::<BTreeSet<_>>();
  for candidate in find_aborted_compactions(trace, session.id())? {
    if candidate_ids.insert(candidate.0.envelope.meta.event_id.clone()) {
      candidates.push(candidate);
    }
  }
  for (start, summary_event_id) in candidates {
    let start_event_id = start.envelope.meta.event_id.clone();
    let marker_present = records.iter().any(|record| {
      matches!(
        record,
        SessionRecord::Compaction(compaction)
          if compaction.aborted
            && compaction.start_event_id.as_ref() == Some(&start_event_id)
      )
    });
    if !trace
      .iter()
      .any(|entry| is_compaction_abort(entry, &start_event_id))
    {
      let mut meta = EventMeta::new(session.id().clone(), start.envelope.meta.trace_id.clone());
      meta.turn_id = start.envelope.meta.turn_id.clone();
      meta.model_epoch = start.envelope.meta.model_epoch;
      meta.model = start.envelope.meta.model.clone();
      meta.parent_event_id = Some(start_event_id.clone());
      let mut diagnostic = EventEnvelope::new(
        meta,
        AgentEvent::Diagnostic(Diagnostic {
          level: DiagnosticLevel::Warn,
          message: compaction_abort_message(&start_event_id),
        }),
      );
      session.emit(&mut diagnostic)?;
    }
    if !marker_present {
      let level = match &start.envelope.event {
        AgentEvent::ContextCompactionStarted(started) => started.level,
        _ => unreachable!("incomplete compaction points to a start event"),
      };
      let marker = SessionRecord::Compaction(rupi_core::SessionCompactionRecord {
        context_epoch: 0,
        level,
        removed_messages: 0,
        retained_from: 0,
        retained_messages: 0,
        summary_present: false,
        replaces_from: None,
        replaces_through: None,
        aborted: true,
        start_event_id: Some(start_event_id.clone()),
        summary_event_id: summary_event_id.clone(),
      });
      session.log.append(&marker)?;
      records.push(marker);
    }
    let pending_ids = session
      .wal
      .pending()?
      .into_iter()
      .filter(|intent| {
        intent.tx_id == start_event_id
          || summary_event_id
            .as_ref()
            .is_some_and(|summary| intent.tx_id == *summary)
      })
      .map(|intent| intent.tx_id)
      .collect::<Vec<_>>();
    for tx_id in pending_ids {
      session.wal.commit(&tx_id)?;
    }
  }
  Ok(())
}

fn recover_incomplete_checkpoints(
  session: &mut Session,
  trace: &[rupi_core::TraceEntry],
  records: &[SessionRecord],
) -> Result<(), StoreError> {
  let incomplete = find_incomplete_checkpoints(trace, session.id())?;
  if incomplete.is_empty() {
    return Ok(());
  }
  validate_checkpoint_capsules(&session.layout, records, session.id())?;
  for boundary in incomplete {
    let barrier_index = records.iter().position(|record| {
      matches!(
        record,
        SessionRecord::CheckpointBarrier(barrier)
          if barrier.checkpoint_id == boundary.created.checkpoint_id
      )
    });
    let Some(barrier_index) = barrier_index else {
      return Err(StoreError::Invalid(format!(
        "session {} checkpoint {} has no matching barrier; resume requires recovery",
        session.id(),
        boundary.created.checkpoint_id
      )));
    };
    // The L3 completion's retained count describes the projection immediately
    // before the barrier, not only records appended afterward. Prefix
    // checkpoints deliberately leave the current-turn suffix before their
    // barrier, so reconstruct that bounded prefix projection before deriving
    // the deterministic count.
    let pre_barrier_report = crate::jsonl::ReadReport {
      items: records[..barrier_index].to_vec(),
      malformed: 0,
      first_malformed_line: None,
    };
    let pre_barrier = crate::session_log::restore_from_report(session.path(), &pre_barrier_report)?;
    let removed_messages = usize::try_from(boundary.created.summarized_events).map_err(|_| {
      StoreError::Invalid("checkpoint summarized message count exceeds durable limit".into())
    })?;
    if removed_messages > pre_barrier.messages.len() {
      return Err(StoreError::Invalid(format!(
        "session {} checkpoint {} removes {} messages but only {} precede its barrier; resume requires recovery",
        session.id(),
        boundary.created.checkpoint_id,
        removed_messages,
        pre_barrier.messages.len()
      )));
    }
    let retained_messages = u32::try_from(
      pre_barrier
        .messages
        .len()
        .saturating_sub(removed_messages)
        .checked_add(1)
        .ok_or_else(|| {
          StoreError::Invalid("checkpoint retained message count is exhausted".into())
        })?,
    )
    .map_err(|_| StoreError::Invalid("checkpoint retained message count is exhausted".into()))?;
    let removed_messages = u32::try_from(removed_messages).map_err(|_| {
      StoreError::Invalid("checkpoint summarized message count exceeds durable limit".into())
    })?;
    let mut meta = EventMeta::new(
      session.id().clone(),
      boundary.entry.envelope.meta.trace_id.clone(),
    );
    meta.turn_id = boundary.entry.envelope.meta.turn_id.clone();
    meta.model_epoch = boundary.entry.envelope.meta.model_epoch;
    meta.model = boundary.entry.envelope.meta.model.clone();
    meta.parent_event_id = Some(boundary.entry.envelope.meta.event_id.clone());
    let mut completion = EventEnvelope::new(
      meta,
      AgentEvent::ContextCompactionCompleted(rupi_core::ContextCompactionCompleted {
        level: rupi_core::ContextLevel::L3Checkpoint,
        removed_messages,
        retained_messages,
        context_epoch: boundary.created.context_epoch,
      }),
    );
    let projection = SessionRecord::Compaction(rupi_core::SessionCompactionRecord {
      context_epoch: boundary.created.context_epoch,
      level: rupi_core::ContextLevel::L3Checkpoint,
      removed_messages,
      retained_from: 0,
      retained_messages,
      summary_present: false,
      replaces_from: None,
      replaces_through: None,
      aborted: false,
      start_event_id: None,
      summary_event_id: None,
    });
    session.emit_transaction(&mut completion, Some(projection), false)?;
  }
  Ok(())
}

fn event_kind(event: &AgentEvent) -> &'static str {
  match event {
    AgentEvent::SessionStarted(_) => "session_started",
    AgentEvent::UserMessage(_) => "user_message",
    AgentEvent::ModelRequestStarted(_) => "model_request_started",
    AgentEvent::ReasoningDelta(_) => "reasoning_delta",
    AgentEvent::AssistantDelta(_) => "assistant_delta",
    AgentEvent::ModelRequestCompleted(_) => "model_request_completed",
    AgentEvent::ModelRetry(_) => "model_retry",
    AgentEvent::ModelFailover(_) => "model_failover",
    AgentEvent::ModelEpochStarted(_) => "model_epoch_started",
    AgentEvent::ToolRequested(_) => "tool_requested",
    AgentEvent::ToolStarted(_) => "tool_started",
    AgentEvent::ToolCompleted(_) => "tool_completed",
    AgentEvent::ToolFailed(_) => "tool_failed",
    AgentEvent::ToolUnknown(_) => "tool_unknown",
    AgentEvent::ExternalContextRetrieved(_) => "external_context_retrieved",
    AgentEvent::ContextReduced(_) => "context_reduced",
    AgentEvent::ContextSummary => "context_summary",
    AgentEvent::ContextCompactionStarted(_) => "context_compaction_started",
    AgentEvent::ContextCompactionEpoch(_) => "context_compaction_epoch",
    AgentEvent::ContextCompactionCompleted(_) => "context_compaction_completed",
    AgentEvent::CheckpointCreated(_) => "checkpoint_created",
    AgentEvent::TurnCompleted(_) => "turn_completed",
    AgentEvent::Diagnostic(_) => "diagnostic",
    AgentEvent::SessionEnded(_) => "session_ended",
  }
}

fn read_checkpoint_capsule(
  path: &Path,
  session: &SessionId,
  checkpoint: &CheckpointId,
) -> Result<ContextCapsule, StoreError> {
  let metadata = std::fs::symlink_metadata(path).map_err(|error| {
    StoreError::Invalid(format!(
      "session {session} checkpoint {checkpoint} capsule is unreadable: {error}; resume requires recovery"
    ))
  })?;
  if !metadata.file_type().is_file() {
    return Err(StoreError::Invalid(format!(
      "session {session} checkpoint {checkpoint} capsule is not a regular file; resume requires recovery"
    )));
  }
  let size = metadata.len();
  if size > MAX_CAPSULE_BYTES {
    return Err(StoreError::Invalid(format!(
      "session {session} checkpoint {checkpoint} capsule exceeds the {MAX_CAPSULE_BYTES}-byte bound; resume requires recovery"
    )));
  }
  let bytes = std::fs::read(path).map_err(|error| {
    StoreError::Invalid(format!(
      "session {session} checkpoint {checkpoint} capsule is unreadable: {error}; resume requires recovery"
    ))
  })?;
  if bytes.len() as u64 > MAX_CAPSULE_BYTES {
    return Err(StoreError::Invalid(format!(
      "session {session} checkpoint {checkpoint} capsule exceeds the {MAX_CAPSULE_BYTES}-byte bound; resume requires recovery"
    )));
  }
  serde_json::from_slice::<ContextCapsule>(&bytes).map_err(|error| {
    StoreError::Invalid(format!(
      "session {session} checkpoint {checkpoint} capsule is invalid: {error}; resume requires recovery"
    ))
  })
}

fn validate_checkpoint_capsules(
  layout: &StateLayout,
  records: &[SessionRecord],
  session: &SessionId,
) -> Result<(), StoreError> {
  let mut count = 0usize;
  let mut total_bytes = 0u64;
  for record in records {
    let SessionRecord::CheckpointBarrier(barrier) = record else {
      continue;
    };
    if !valid_checkpoint_id(&barrier.checkpoint_id) {
      return Err(StoreError::Invalid(format!(
        "session {session} checkpoint {} has an invalid identity; resume requires recovery",
        barrier.checkpoint_id
      )));
    }
    let expected_path = format!("checkpoints/{}.json", barrier.checkpoint_id);
    if barrier.capsule_path != expected_path {
      return Err(StoreError::Invalid(format!(
        "session {session} checkpoint {} has an invalid capsule path; resume requires recovery",
        barrier.checkpoint_id
      )));
    }
    count = count.checked_add(1).ok_or_else(|| {
      StoreError::Invalid(format!(
        "session {session} checkpoint count is exhausted; resume requires recovery"
      ))
    })?;
    if count > MAX_CHECKPOINT_COUNT {
      return Err(StoreError::Invalid(format!(
        "session {session} contains more than {MAX_CHECKPOINT_COUNT} checkpoints; resume requires recovery"
      )));
    }
    let path = layout.checkpoint_path(session, &barrier.checkpoint_id);
    let size = std::fs::metadata(&path).map_err(|error| {
      StoreError::Invalid(format!(
        "session {session} checkpoint {} capsule is unreadable: {error}; resume requires recovery",
        barrier.checkpoint_id
      ))
    })?.len();
    total_bytes = total_bytes.checked_add(size).ok_or_else(|| {
      StoreError::Invalid(format!(
        "session {session} checkpoint bytes are exhausted; resume requires recovery"
      ))
    })?;
    if total_bytes > MAX_CHECKPOINT_TOTAL_BYTES {
      return Err(StoreError::Invalid(format!(
        "session {session} checkpoint capsules exceed the {MAX_CHECKPOINT_TOTAL_BYTES}-byte aggregate bound; resume requires recovery"
      )));
    }
    let capsule = read_checkpoint_capsule(&path, session, &barrier.checkpoint_id)?;
    if capsule.version != rupi_core::context::CAPSULE_SCHEMA_VERSION
      || barrier.capsule_version != rupi_core::context::CAPSULE_SCHEMA_VERSION
      || capsule != barrier.capsule
    {
      return Err(StoreError::Invalid(format!(
        "session {session} checkpoint {} capsule disagrees with its barrier; resume requires recovery",
        barrier.checkpoint_id
      )));
    }
  }
  Ok(())
}

fn committed_checkpoint_ids(path: &Path) -> Result<BTreeSet<CheckpointId>, StoreError> {
  let report = SessionLog::read(path)?;
  if report.malformed > 0 {
    return Err(StoreError::Invalid(format!(
      "{} contains {} malformed records while listing checkpoints",
      path.display(),
      report.malformed
    )));
  }
  let mut ids = BTreeSet::new();
  for record in report.items {
    let SessionRecord::CheckpointBarrier(barrier) = record else {
      continue;
    };
    if !valid_checkpoint_id(&barrier.checkpoint_id) {
      return Err(StoreError::Invalid(format!(
        "{} contains an invalid checkpoint identity",
        path.display()
      )));
    }
    if !ids.insert(barrier.checkpoint_id.clone()) {
      return Err(StoreError::Invalid(format!(
        "{} contains duplicate checkpoint {}",
        path.display(),
        barrier.checkpoint_id
      )));
    }
  }
  Ok(ids)
}

fn valid_checkpoint_id(id: &CheckpointId) -> bool {
  let value = id.as_str();
  !value.is_empty()
    && value != "."
    && value != ".."
    && !value.contains('/')
    && !value.contains('\\')
    && !value.chars().any(char::is_control)
}

fn validate_trace_integrity(
  entries: &[rupi_core::TraceEntry],
  session: &SessionId,
) -> Result<(), StoreError> {
  let mut last_seq = None;
  let mut event_ids = BTreeSet::new();
  for entry in entries {
    if entry.envelope.v != rupi_core::event::EVENT_SCHEMA_VERSION {
      return Err(StoreError::Invalid(format!(
        "session {session} trace event {} has unsupported schema version {}; resume requires recovery",
        entry.envelope.meta.event_id, entry.envelope.v
      )));
    }
    if entry.envelope.meta.session_id != *session {
      return Err(StoreError::Invalid(format!(
        "session {session} trace contains event {} from another session; resume requires recovery",
        entry.envelope.meta.event_id
      )));
    }
    if entry.envelope.meta.event_id.as_str().is_empty()
      || entry.envelope.meta.trace_id.as_str().is_empty()
      || entry.envelope.meta.span_id.as_str().is_empty()
      || entry
        .envelope
        .meta
        .turn_id
        .as_ref()
        .is_some_and(|turn| turn.as_str().is_empty())
      || entry
        .envelope
        .meta
        .parent_event_id
        .as_ref()
        .is_some_and(|parent| parent.as_str().is_empty())
    {
      return Err(StoreError::Invalid(format!(
        "session {session} trace event {} has incomplete identity metadata; resume requires recovery",
        entry.envelope.meta.event_id
      )));
    }
    let seq = entry.envelope.meta.seq.ok_or_else(|| {
      StoreError::Invalid(format!(
        "session {session} trace event {} has no canonical sequence; resume requires recovery",
        entry.envelope.meta.event_id
      ))
    })?;
    if seq.0 == 0
      || last_seq
        .is_some_and(|previous| seq <= previous || previous.0.checked_add(1) != Some(seq.0))
    {
      return Err(StoreError::Invalid(format!(
        "session {session} trace has a non-contiguous canonical sequence at event {}; resume requires recovery",
        entry.envelope.meta.event_id
      )));
    }
    if let Some(parent) = &entry.envelope.meta.parent_event_id
      && (parent == &entry.envelope.meta.event_id || !event_ids.contains(parent))
    {
      return Err(StoreError::Invalid(format!(
        "session {session} trace event {} has an invalid causal parent {}; resume requires recovery",
        entry.envelope.meta.event_id, parent
      )));
    }
    if !event_ids.insert(entry.envelope.meta.event_id.clone()) {
      return Err(StoreError::Invalid(format!(
        "session {session} trace contains duplicate event {}; resume requires recovery",
        entry.envelope.meta.event_id
      )));
    }
    last_seq = Some(seq);
  }
  Ok(())
}

fn validate_trace_payloads(
  entries: &[rupi_core::TraceEntry],
  blobs: &BlobStore,
  session: &SessionId,
) -> Result<(), StoreError> {
  for entry in entries {
    for field in &entry.externalized {
      if field.bytes == 0 {
        return Err(StoreError::Invalid(format!(
          "session {session} trace event {} externalizes an empty field; resume requires recovery",
          entry.envelope.meta.event_id
        )));
      }
      let bytes = blobs.get_relative_verified(&field.reference).map_err(|error| {
        StoreError::Invalid(format!(
          "session {session} trace event {} has an invalid externalized field {}: {error}; resume requires recovery",
          entry.envelope.meta.event_id, field.field
        ))
      })?;
      if bytes.len() as u64 != field.bytes || std::str::from_utf8(&bytes).is_err() {
        return Err(StoreError::Invalid(format!(
          "session {session} trace event {} has an invalid externalized field {}; resume requires recovery",
          entry.envelope.meta.event_id, field.field
        )));
      }
    }
    let check_blob = |label: &str, blob: &BlobRef| -> Result<(), StoreError> {
      if !blob.is_well_formed() || !blobs.verify(blob)? {
        return Err(StoreError::Invalid(format!(
          "session {session} trace event {} has an invalid {label} blob; resume requires recovery",
          entry.envelope.meta.event_id
        )));
      }
      Ok(())
    };
    match &entry.envelope.event {
      AgentEvent::ContextCompactionEpoch(epoch) => {
        if let Some(blob) = &epoch.summary {
          check_blob("compaction summary", blob)?;
        }
      }
      AgentEvent::ContextReduced(reduced) => {
        if reduced.blob.is_none() && reduced.recovery_ref.is_some() {
          return Err(StoreError::Invalid(format!(
            "session {session} trace event {} has a recovery reference without a blob; resume requires recovery",
            entry.envelope.meta.event_id
          )));
        }
        if reduced.removed_messages > 0 && reduced.blob.is_none() {
          return Err(StoreError::Invalid(format!(
            "session {session} trace event {} is missing its reduction blob; resume requires recovery",
            entry.envelope.meta.event_id
          )));
        }
        if let Some(blob) = &reduced.blob {
          if !blob.is_well_formed() {
            return Err(StoreError::Invalid(format!(
              "session {session} trace event {} has an invalid reduction blob; resume requires recovery",
              entry.envelope.meta.event_id
            )));
          }
          if !blobs.verify(blob)? {
            return Err(StoreError::Invalid(format!(
              "session {session} trace event {} has a missing or invalid reduction blob; resume requires recovery",
              entry.envelope.meta.event_id
            )));
          }
          if reduced.recovery_ref.as_deref() != Some(blob.recovery_ref().as_str()) {
            return Err(StoreError::Invalid(format!(
              "session {session} trace event {} has a mismatched reduction recovery reference; resume requires recovery",
              entry.envelope.meta.event_id
            )));
          }
        }
      }
      AgentEvent::ToolCompleted(completed) => {
        if let Some(blob) = &completed.blob {
          check_blob("tool result", blob)?;
        }
      }
      _ => {}
    }
  }
  Ok(())
}

#[derive(Debug, Clone)]
struct OpenModelRequest {
  key: (Option<TurnId>, u32, ModelRef),
  start: rupi_core::TraceEntry,
}

/// Validate request metadata and return starts that need safe crash recovery.
/// The caller decides whether an open request is normalized or rejected.
fn inspect_model_request_lifecycles(
  entries: &[rupi_core::TraceEntry],
  session: &SessionId,
) -> Result<Vec<rupi_core::TraceEntry>, StoreError> {
  let mut open = Vec::<OpenModelRequest>::new();
  for entry in entries {
    match &entry.envelope.event {
      AgentEvent::ModelRequestStarted(started) => {
        if entry.envelope.meta.turn_id.is_none() {
          return Err(StoreError::Invalid(format!(
            "session {session} model request start has no turn identity; resume requires recovery"
          )));
        }
        if entry
          .envelope
          .meta
          .model_epoch
          .is_some_and(|epoch| epoch != started.epoch)
          || entry
            .envelope
            .meta
            .model
            .as_ref()
            .is_some_and(|model| model != &started.model)
        {
          return Err(StoreError::Invalid(format!(
            "session {session} model request start metadata disagrees with its event; resume requires recovery"
          )));
        }
        let key = (
          entry.envelope.meta.turn_id.clone(),
          started.epoch,
          started.model.clone(),
        );
        if open.iter().any(|candidate| candidate.key == key) {
          return Err(StoreError::Invalid(format!(
            "session {session} has overlapping model request lifecycles; resume requires recovery"
          )));
        }
        open.push(OpenModelRequest {
          key,
          start: entry.clone(),
        });
      }
      AgentEvent::ModelRequestCompleted(completed) => {
        if entry.envelope.meta.turn_id.is_none() {
          return Err(StoreError::Invalid(format!(
            "session {session} model request completion has no turn identity; resume requires recovery"
          )));
        }
        if entry
          .envelope
          .meta
          .model_epoch
          .is_some_and(|epoch| epoch != completed.epoch)
          || entry
            .envelope
            .meta
            .model
            .as_ref()
            .is_some_and(|model| model != &completed.model)
        {
          return Err(StoreError::Invalid(format!(
            "session {session} model request completion metadata disagrees with its event; resume requires recovery"
          )));
        }
        let key = (
          entry.envelope.meta.turn_id.clone(),
          completed.epoch,
          completed.model.clone(),
        );
        let Some(index) = open.iter().rposition(|candidate| candidate.key == key) else {
          return Err(StoreError::Invalid(format!(
            "session {session} has a model request completion without a matching start; resume requires recovery"
          )));
        };
        open.remove(index);
      }
      _ => {}
    }
  }
  Ok(open.into_iter().map(|request| request.start).collect())
}

fn validate_model_request_lifecycles(
  entries: &[rupi_core::TraceEntry],
  session: &SessionId,
) -> Result<(), StoreError> {
  if inspect_model_request_lifecycles(entries, session)?.is_empty() {
    Ok(())
  } else {
    Err(StoreError::Invalid(format!(
      "session {session} trace contains an incomplete model request lifecycle; resume requires recovery"
    )))
  }
}

fn validate_compaction_lifecycles(
  entries: &[rupi_core::TraceEntry],
  session: &SessionId,
) -> Result<(), StoreError> {
  #[derive(Debug)]
  struct OpenCompaction {
    index: usize,
    level: rupi_core::ContextLevel,
    turn_id: Option<TurnId>,
  }

  let mut open: Option<OpenCompaction> = None;
  for (index, entry) in entries.iter().enumerate() {
    match &entry.envelope.event {
      AgentEvent::ContextCompactionStarted(started) => {
        if started.level == rupi_core::ContextLevel::L3Checkpoint {
          return Err(StoreError::Invalid(format!(
            "session {session} has a checkpoint compaction start without a checkpoint barrier; resume requires recovery"
          )));
        }
        if started.level == rupi_core::ContextLevel::L0Payload || open.is_some() {
          return Err(StoreError::Invalid(format!(
            "session {session} has an ambiguous context-compaction lifecycle; resume requires recovery"
          )));
        }
        open = Some(OpenCompaction {
          index,
          level: started.level,
          turn_id: entry.envelope.meta.turn_id.clone(),
        });
      }
      AgentEvent::ContextCompactionCompleted(completed)
        if completed.level != rupi_core::ContextLevel::L3Checkpoint =>
      {
        let Some(start) = open.take() else {
          return Err(StoreError::Invalid(format!(
            "session {session} has a compaction completion without a matching start; resume requires recovery"
          )));
        };
        if start.level != completed.level || start.turn_id != entry.envelope.meta.turn_id {
          return Err(StoreError::Invalid(format!(
            "session {session} compaction completion does not match its start; resume requires recovery"
          )));
        }
        if completed.context_epoch == 0 || completed.removed_messages == 0 {
          return Err(StoreError::Invalid(format!(
            "session {session} has an invalid context-compaction completion; resume requires recovery"
          )));
        }
        let completion_seq = entry.envelope.meta.seq.ok_or_else(|| {
          StoreError::Invalid(format!(
            "session {session} compaction completion has no canonical sequence; resume requires recovery"
          ))
        })?;
        let summary: Vec<_> = entries
          .iter()
          .skip(start.index + 1)
          .take(index.saturating_sub(start.index + 1))
          .filter(|candidate| {
            candidate.envelope.meta.turn_id == start.turn_id
              && matches!(candidate.envelope.event, AgentEvent::ContextSummary)
          })
          .collect();
        let epochs: Vec<_> = entries
          .iter()
          .skip(start.index + 1)
          .take(index.saturating_sub(start.index + 1))
          .filter_map(|candidate| match &candidate.envelope.event {
            AgentEvent::ContextCompactionEpoch(epoch)
              if candidate.envelope.meta.turn_id == start.turn_id
                && epoch.context_epoch == completed.context_epoch =>
            {
              Some(epoch)
            }
            _ => None,
          })
          .collect();
        if summary.len() != 1 || epochs.len() != 1 {
          return Err(StoreError::Invalid(format!(
            "session {session} compaction epoch {} has incomplete summary boundaries; resume requires recovery",
            completed.context_epoch
          )));
        }
        let epoch = epochs[0];
        if epoch.replaces_from.0 == 0 || epoch.replaces_from > epoch.replaces_through {
          return Err(StoreError::Invalid(format!(
            "session {session} compaction epoch {} has invalid canonical bounds; resume requires recovery",
            completed.context_epoch
          )));
        }
        let summary_seq = summary[0].envelope.meta.seq.ok_or_else(|| {
          StoreError::Invalid(format!(
            "session {session} compaction summary has no canonical sequence; resume requires recovery"
          ))
        })?;
        let epoch_seq = entries
          .iter()
          .skip(start.index + 1)
          .take(index.saturating_sub(start.index + 1))
          .find_map(|candidate| {
            matches!(
              &candidate.envelope.event,
              AgentEvent::ContextCompactionEpoch(record)
                if record.context_epoch == completed.context_epoch
            )
            .then_some(candidate.envelope.meta.seq)
            .flatten()
          })
          .ok_or_else(|| {
            StoreError::Invalid(format!(
              "session {session} compaction epoch {} has no canonical sequence; resume requires recovery",
              completed.context_epoch
            ))
          })?;
        if !(summary_seq < epoch_seq && epoch_seq < completion_seq) {
          return Err(StoreError::Invalid(format!(
            "session {session} compaction epoch {} is out of order; resume requires recovery",
            completed.context_epoch
          )));
        }
      }
      AgentEvent::Diagnostic(_) if open.is_some() => {
        let start_index = open.as_ref().expect("checked above").index;
        let start_id = &entries[start_index].envelope.meta.event_id;
        if is_compaction_abort(entry, start_id) {
          open = None;
        }
      }
      AgentEvent::ContextCompactionCompleted(_) => {}
      _ => {}
    }
  }
  if open.is_some() {
    return Err(StoreError::Invalid(format!(
      "session {session} trace contains an incomplete context-compaction lifecycle; resume requires recovery"
    )));
  }
  Ok(())
}

/// Checkpoint completion is an L3 boundary without an ordinary compaction start.
/// A durable checkpoint event must therefore be joined to the completion that
/// closes its epoch; otherwise a crash between the barrier and reset could let a
/// later writer reuse an ambiguous context epoch.
fn validate_checkpoint_lifecycles(
  entries: &[rupi_core::TraceEntry],
  session: &SessionId,
) -> Result<(), StoreError> {
  let mut epochs = BTreeSet::new();
  let mut checkpoint_ids = BTreeSet::new();
  let mut pending: Option<(u32, Option<TurnId>, u64)> = None;
  for entry in entries {
    match &entry.envelope.event {
      AgentEvent::CheckpointCreated(created) => {
        if !checkpoint_ids.insert(created.checkpoint_id.clone()) {
          return Err(StoreError::Invalid(format!(
            "session {session} repeats checkpoint {}; resume requires recovery",
            created.checkpoint_id
          )));
        }
        if pending.is_some() {
          return Err(StoreError::Invalid(format!(
            "session {session} has overlapping checkpoint lifecycles; resume requires recovery"
          )));
        }
        if created.context_epoch == 0 {
          continue;
        }
        if !epochs.insert(created.context_epoch) {
          return Err(StoreError::Invalid(format!(
            "session {session} reuses checkpoint context epoch {}; resume requires recovery",
            created.context_epoch
          )));
        }
        pending = Some((
          created.context_epoch,
          entry.envelope.meta.turn_id.clone(),
          created.summarized_events,
        ));
      }
      AgentEvent::ContextCompactionCompleted(completed)
        if completed.level == rupi_core::ContextLevel::L3Checkpoint
          && completed.context_epoch > 0 =>
      {
        let Some((epoch, turn_id, summarized_events)) = pending.take() else {
          return Err(StoreError::Invalid(format!(
            "session {session} checkpoint completion has no preceding checkpoint; resume requires recovery"
          )));
        };
        if epoch != completed.context_epoch || turn_id != entry.envelope.meta.turn_id {
          return Err(StoreError::Invalid(format!(
            "session {session} checkpoint completion does not match its boundary; resume requires recovery"
          )));
        }
        if u64::from(completed.removed_messages) != summarized_events {
          return Err(StoreError::Invalid(format!(
            "session {session} checkpoint epoch {} removes {} messages but records {}; resume requires recovery",
            completed.context_epoch, completed.removed_messages, summarized_events
          )));
        }
      }
      _ => {}
    }
  }
  if pending.is_some() {
    return Err(StoreError::Invalid(format!(
      "session {session} checkpoint has no completion; resume requires recovery"
    )));
  }
  Ok(())
}

/// Check the trace/projection join. Low-level callers may still write synthetic
/// semantic records without a trace; those unlinked records are checked only
/// when they claim a canonical sequence. Canonical events that imply a runtime
/// projection are never silently accepted without their corresponding record.
fn validate_projection_alignment(
  entries: &[rupi_core::TraceEntry],
  records: &[SessionRecord],
  blobs: &BlobStore,
  session: &SessionId,
  _checkpoint_seq: Option<EventSeq>,
) -> Result<(), StoreError> {
  let linked = |record: &SessionRecord| match record {
    SessionRecord::Message(message) => message.seq,
    SessionRecord::Reduction(reduction) => reduction.seq,
    _ => None,
  };
  let linked_records: Vec<&SessionRecord> = records
    .iter()
    .filter(|record| linked(record).is_some())
    .collect();
  // Check every canonical compaction boundary, including epochs before the
  // latest checkpoint. Checkpoint filtering only changes the model-visible
  // window; it never erases the semantic projection needed to prove history.
  let scoped_compactions: Vec<&rupi_core::SessionCompactionRecord> = records
    .iter()
    .filter_map(|record| match record {
      SessionRecord::Compaction(compaction)
        if !compaction.aborted
          && compaction.replaces_from.is_some()
          && compaction.replaces_through.is_some() =>
      {
        Some(compaction)
      }
      _ => None,
    })
    .collect();
  let has_message = |event_id: &EventId| {
    linked_records.iter().any(
      |record| matches!(record, SessionRecord::Message(message) if message.event_id == *event_id),
    )
  };
  let fail = |entry: &rupi_core::TraceEntry| {
    StoreError::Invalid(format!(
      "session {session} canonical {} event {} has no semantic projection; resume requires recovery",
      event_kind(&entry.envelope.event),
      entry.envelope.meta.event_id
    ))
  };
  let aborted_events = aborted_compaction_event_ids(entries);

  for entry in entries {
    // A checkpoint changes the model-visible window, not the durable joins. All
    // message and lifecycle facts remain auditable against their projections.
    match &entry.envelope.event {
      AgentEvent::ContextSummary | AgentEvent::ContextCompactionEpoch(_)
        if aborted_events.contains(&entry.envelope.meta.event_id) => {}
      AgentEvent::UserMessage(_)
      | AgentEvent::ExternalContextRetrieved(_)
      | AgentEvent::ContextSummary
      | AgentEvent::ToolCompleted(_) => {
        if !has_message(&entry.envelope.meta.event_id) {
          return Err(fail(entry));
        }
      }
      AgentEvent::ToolFailed(failed) => {
        // The runtime records a failed, never-started request without a
        // ToolResult message when a provider response itself fails. Once a
        // ToolStarted boundary exists, however, a terminal failure must carry
        // its model-visible projection.
        let started = entries.iter().any(|candidate| {
          candidate.envelope.meta.seq < entry.envelope.meta.seq
            && matches!(
              &candidate.envelope.event,
              AgentEvent::ToolStarted(started) if started.call_id == failed.call_id
            )
        });
        if started && !has_message(&entry.envelope.meta.event_id) {
          return Err(fail(entry));
        }
      }
      AgentEvent::ToolUnknown(unknown) => {
        let started = entries.iter().any(|candidate| {
          candidate.envelope.meta.seq < entry.envelope.meta.seq
            && matches!(
              &candidate.envelope.event,
              AgentEvent::ToolStarted(started) if started.call_id == unknown.call_id
            )
        });
        if started && !has_message(&entry.envelope.meta.event_id) {
          return Err(fail(entry));
        }
      }
      AgentEvent::ContextReduced(reduced) if reduced.removed_messages > 0 => {
        let Some(projection) = linked_records.iter().find_map(|record| match record {
          SessionRecord::Reduction(reduction)
            if reduction.event_id == entry.envelope.meta.event_id =>
          {
            Some(reduction)
          }
          _ => None,
        }) else {
          return Err(fail(entry));
        };
        if projection.reason != reduced.reason
          || projection.removed_messages != reduced.removed_messages
          || projection.retained_messages != reduced.retained_messages
        {
          return Err(StoreError::Invalid(format!(
            "session {session} reduction projection for event {} disagrees with its canonical trace; resume requires recovery",
            entry.envelope.meta.event_id
          )));
        }
      }
      AgentEvent::ContextCompactionEpoch(epoch) => {
        let projected = scoped_compactions.iter().any(|compaction| {
          compaction.context_epoch == epoch.context_epoch
            && compaction.replaces_from == Some(epoch.replaces_from)
            && compaction.replaces_through == Some(epoch.replaces_through)
        });
        if !projected {
          return Err(fail(entry));
        }
      }
      AgentEvent::ContextCompactionCompleted(completed) => {
        let projected = records.iter().any(|record| {
          matches!(
            record,
            SessionRecord::Compaction(compaction)
              if compaction.context_epoch == completed.context_epoch
                && compaction.level == completed.level
                && compaction.removed_messages == completed.removed_messages
                && compaction.retained_messages == completed.retained_messages
          )
        });
        if !projected {
          return Err(fail(entry));
        }
      }
      AgentEvent::ModelEpochStarted(started) => {
        if entry
          .envelope
          .meta
          .model_epoch
          .is_some_and(|epoch| epoch != started.epoch)
          || entry
            .envelope
            .meta
            .model
            .as_ref()
            .is_some_and(|model| model != &started.model)
        {
          return Err(StoreError::Invalid(format!(
            "session {session} model epoch metadata disagrees with its event; resume requires recovery"
          )));
        }
        let projected = records.iter().any(|record| {
          matches!(
            record,
            SessionRecord::Epoch(epoch)
              if epoch.epoch == started.epoch
                && epoch.model == started.model
                && epoch.reason == started.reason
          )
        });
        if !projected {
          return Err(fail(entry));
        }
      }
      AgentEvent::CheckpointCreated(created) => {
        let projected = records.iter().any(|record| {
          matches!(
            record,
            SessionRecord::CheckpointBarrier(barrier)
              if barrier.checkpoint_id == created.checkpoint_id
                && barrier.context_epoch == created.context_epoch
                && barrier.capsule_version == created.capsule_version
                && barrier.capsule_path == created.path
                && barrier.capsule.version == created.capsule_version
          )
        });
        if !projected {
          return Err(fail(entry));
        }
      }
      AgentEvent::ModelRequestCompleted(completed)
        if completed.finish_reason.is_some()
          && completed.finish_reason.as_deref() != Some("abandoned") =>
      {
        let completion_seq = entry.envelope.meta.seq;
        let request_start = entries
          .iter()
          .filter_map(|candidate| {
            let candidate_seq = candidate.envelope.meta.seq?;
            let completion_seq = completion_seq?;
            if candidate_seq >= completion_seq
              || candidate.envelope.meta.turn_id != entry.envelope.meta.turn_id
              || candidate.envelope.meta.model_epoch != entry.envelope.meta.model_epoch
            {
              return None;
            }
            matches!(
              &candidate.envelope.event,
              AgentEvent::ModelRequestStarted(_)
            )
            .then_some(candidate_seq)
          })
          .max();
        let has_deltas = entries.iter().any(|candidate| {
          let Some(candidate_seq) = candidate.envelope.meta.seq else {
            return false;
          };
          let Some(completion_seq) = completion_seq else {
            return false;
          };
          candidate_seq > request_start.unwrap_or(EventSeq(0))
            && candidate_seq < completion_seq
            && candidate.envelope.meta.turn_id == entry.envelope.meta.turn_id
            && candidate.envelope.meta.model_epoch == entry.envelope.meta.model_epoch
            && matches!(&candidate.envelope.event, AgentEvent::AssistantDelta(_))
        });
        if has_deltas {
          let projected = linked_records.iter().any(|record| {
            let SessionRecord::Message(message) = record else {
              return false;
            };
            message.role == Role::Assistant
              && entries.iter().any(|candidate| {
                candidate.envelope.meta.event_id == message.event_id
                  && matches!(
                    &candidate.envelope.event,
                    AgentEvent::AssistantDelta(_) | AgentEvent::ModelRequestCompleted(_)
                  )
              })
          });
          if !projected {
            return Err(fail(entry));
          }
        }
      }
      _ => {}
    }
  }

  for record in records.iter() {
    let SessionRecord::Message(message) = record else {
      continue;
    };
    for call in message
      .message
      .content
      .iter()
      .filter_map(|block| match block {
        rupi_core::ContentBlock::ToolCall(call) => Some(call),
        _ => None,
      })
    {
      let mut requested = false;
      for entry in entries {
        let event = restore_externalized_event_from_blobs(entry, blobs, session)?;
        if let AgentEvent::ToolRequested(candidate) = event
          && candidate.call_id == call.id
          && candidate.name == call.name
          && candidate.arguments == call.arguments
          && entry.envelope.meta.turn_id == Some(message.turn_id.clone())
          && entry
            .envelope
            .meta
            .model_epoch
            .is_none_or(|epoch| epoch == message.epoch)
          && entry
            .envelope
            .meta
            .model
            .as_ref()
            .is_none_or(|model| model == &message.model)
          && entry
            .envelope
            .meta
            .seq
            .zip(message.seq)
            .is_some_and(|(request_seq, assistant_seq)| request_seq > assistant_seq)
        {
          requested = true;
          break;
        }
      }
      if !requested {
        return Err(StoreError::Invalid(format!(
          "session {session} assistant tool call {} has no canonical ToolRequested event; resume requires recovery",
          call.id
        )));
      }
    }
  }

  // A compaction carrying canonical bounds is trace-backed even though its
  // legacy session-line marker has no event id. Both boundary events must be
  // present; otherwise the projection could silently describe a different
  // history.
  for compaction in scoped_compactions {
    if !compaction.summary_present || compaction.level == rupi_core::ContextLevel::L3Checkpoint {
      return Err(StoreError::Invalid(format!(
        "session {session} has an invalid bounded compaction projection; resume requires recovery"
      )));
    }
    let (Some(from), Some(through)) = (compaction.replaces_from, compaction.replaces_through)
    else {
      unreachable!("scoped compactions have both canonical bounds")
    };
    let has_epoch = entries.iter().any(|entry| {
      matches!(
        &entry.envelope.event,
        AgentEvent::ContextCompactionEpoch(epoch)
          if epoch.context_epoch == compaction.context_epoch
            && epoch.replaces_from == from
            && epoch.replaces_through == through
      )
    });
    let has_completion = entries.iter().any(|entry| {
      matches!(
        &entry.envelope.event,
        AgentEvent::ContextCompactionCompleted(completed)
          if completed.context_epoch == compaction.context_epoch
            && completed.level == compaction.level
            && completed.removed_messages == compaction.removed_messages
            && completed.retained_messages == compaction.retained_messages
      )
    });
    let completion_seq = entries.iter().find_map(|entry| {
      let seq = entry.envelope.meta.seq?;
      matches!(
        &entry.envelope.event,
        AgentEvent::ContextCompactionCompleted(completed)
          if completed.context_epoch == compaction.context_epoch
            && completed.level == compaction.level
            && completed.removed_messages == compaction.removed_messages
            && completed.retained_messages == compaction.retained_messages
      )
      .then_some(seq)
    });
    let summary_entries: Vec<_> = completion_seq
      .into_iter()
      .flat_map(|completion_seq| {
        entries.iter().filter(move |entry| {
          let Some(seq) = entry.envelope.meta.seq else {
            return false;
          };
          seq > through
            && seq < completion_seq
            && entry.envelope.meta.turn_id
              == entries.iter().find_map(|candidate| {
                matches!(
                  &candidate.envelope.event,
                  AgentEvent::ContextCompactionCompleted(completed)
                    if completed.context_epoch == compaction.context_epoch
                      && completed.level == compaction.level
                      && completed.removed_messages == compaction.removed_messages
                      && completed.retained_messages == compaction.retained_messages
                )
                .then_some(candidate.envelope.meta.turn_id.clone())
                .flatten()
              })
            && matches!(entry.envelope.event, AgentEvent::ContextSummary)
        })
      })
      .collect();
    let projection_index = records.iter().position(|record| {
      matches!(
        record,
        SessionRecord::Compaction(candidate)
          if candidate.context_epoch == compaction.context_epoch
            && candidate.level == compaction.level
            && candidate.removed_messages == compaction.removed_messages
            && candidate.retained_messages == compaction.retained_messages
            && candidate.replaces_from == compaction.replaces_from
            && candidate.replaces_through == compaction.replaces_through
      )
    });
    let summary_is_previous_message = projection_index
      .and_then(|index| index.checked_sub(1))
      .and_then(|index| records.get(index))
      .and_then(|record| match record {
        SessionRecord::Message(message) => Some(message),
        _ => None,
      })
      .is_some_and(|message| {
        message.role == Role::User
          && summary_entries.len() == 1
          && message.event_id == summary_entries[0].envelope.meta.event_id
      });
    if !has_epoch || !has_completion || summary_entries.len() != 1 || !summary_is_previous_message {
      return Err(StoreError::Invalid(format!(
        "session {session} compaction epoch {} has no matching canonical summary boundaries; resume requires recovery",
        compaction.context_epoch
      )));
    }
    let summary_message = records
      .iter()
      .position(|record| {
        matches!(
          record,
          SessionRecord::Compaction(candidate)
            if candidate.context_epoch == compaction.context_epoch
              && candidate.level == compaction.level
              && candidate.removed_messages == compaction.removed_messages
              && candidate.retained_messages == compaction.retained_messages
              && candidate.replaces_from == compaction.replaces_from
              && candidate.replaces_through == compaction.replaces_through
        )
      })
      .and_then(|index| index.checked_sub(1))
      .and_then(|index| records.get(index))
      .and_then(|record| match record {
        SessionRecord::Message(message) => Some(message),
        _ => None,
      })
      .expect("summary_is_previous_message proved the projection shape");
    let summary_blob = entries
      .iter()
      .find_map(|entry| match &entry.envelope.event {
        AgentEvent::ContextCompactionEpoch(epoch)
          if epoch.context_epoch == compaction.context_epoch
            && epoch.replaces_from == from
            && epoch.replaces_through == through =>
        {
          epoch.summary.as_ref()
        }
        _ => None,
      });
    if let Some(blob) = summary_blob {
      let bytes = blobs.get(blob).map_err(|error| {
        StoreError::Invalid(format!(
          "session {session} compaction epoch {} summary blob is unreadable: {error}; resume requires recovery",
          compaction.context_epoch
        ))
      })?;
      let text = String::from_utf8(bytes).map_err(|_| {
        StoreError::Invalid(format!(
          "session {session} compaction epoch {} summary blob is not UTF-8; resume requires recovery",
          compaction.context_epoch
        ))
      })?;
      if text != summary_message.message.text() {
        return Err(StoreError::Invalid(format!(
          "session {session} compaction epoch {} summary projection disagrees with its blob; resume requires recovery",
          compaction.context_epoch
        )));
      }
    }
  }

  // The reverse direction catches a semantic line left behind after its trace
  // append was lost or manually removed. Sequence equality is checked as well;
  // an event id copied from another session is not a valid join.
  for record in linked_records {
    let (event_id, seq, message) = match record {
      SessionRecord::Message(message) => (&message.event_id, message.seq, Some(message)),
      SessionRecord::Reduction(reduction) => (&reduction.event_id, reduction.seq, None),
      _ => continue,
    };
    let Some(trace_entry) = entries
      .iter()
      .find(|entry| entry.envelope.meta.event_id == *event_id)
    else {
      return Err(StoreError::Invalid(format!(
        "session {session} semantic event {event_id} has no canonical trace; resume requires recovery"
      )));
    };
    if trace_entry.envelope.meta.seq != seq {
      return Err(StoreError::Invalid(format!(
        "session {session} semantic event {event_id} sequence {:?} disagrees with canonical {:?}",
        seq, trace_entry.envelope.meta.seq
      )));
    }
    if let Some(message) = message {
      validate_message_projection(message, trace_entry, blobs, session)?;
      if message.role == Role::Assistant {
        validate_assistant_message_content(message, entries, blobs, session)?;
      }
    }
  }
  Ok(())
}

fn validate_assistant_message_content(
  message: &SessionMessage,
  entries: &[rupi_core::TraceEntry],
  blobs: &BlobStore,
  session: &SessionId,
) -> Result<(), StoreError> {
  let Some(message_seq) = message.seq else {
    return Ok(());
  };
  let Some(introducing) = entries
    .iter()
    .find(|entry| entry.envelope.meta.event_id == message.event_id)
  else {
    return Ok(());
  };
  let provider_assistant_event =
    matches!(&introducing.envelope.event, AgentEvent::AssistantDelta(_))
      || matches!(
        &introducing.envelope.event,
        AgentEvent::ModelRequestCompleted(_)
      );
  if !provider_assistant_event {
    // `append_message` deliberately permits an application to bind a semantic
    // message to a causal event such as the user prompt.  That event does not
    // carry assistant text to reconstruct, so only provider-produced assistant
    // boundaries get the strict delta/completion check below.
    return Ok(());
  }
  let completion = if matches!(
    &introducing.envelope.event,
    AgentEvent::ModelRequestCompleted(_)
  ) {
    introducing
  } else {
    entries
      .iter()
      .find(|entry| {
        let Some(seq) = entry.envelope.meta.seq else {
          return false;
        };
        seq > message_seq
          && entry.envelope.meta.turn_id == introducing.envelope.meta.turn_id
          && entry.envelope.meta.model_epoch == introducing.envelope.meta.model_epoch
          && matches!(&entry.envelope.event, AgentEvent::ModelRequestCompleted(_))
      })
      .ok_or_else(|| {
        StoreError::Invalid(format!(
          "session {session} assistant message {} has no terminal request; resume requires recovery",
          message.event_id
        ))
      })?
  };
  let completion_seq = completion.envelope.meta.seq.ok_or_else(|| {
    StoreError::Invalid(format!(
      "session {session} assistant completion has no canonical sequence; resume requires recovery"
    ))
  })?;
  let request_start = entries
    .iter()
    .filter_map(|entry| {
      let seq = entry.envelope.meta.seq?;
      (seq < message_seq
        && entry.envelope.meta.turn_id == introducing.envelope.meta.turn_id
        && entry.envelope.meta.model_epoch == introducing.envelope.meta.model_epoch
        && matches!(entry.envelope.event, AgentEvent::ModelRequestStarted(_)))
      .then_some(seq)
    })
    .max()
    .unwrap_or(EventSeq(0));
  let mut expected = String::new();
  for entry in entries.iter().filter(|entry| {
    let Some(seq) = entry.envelope.meta.seq else {
      return false;
    };
    seq >= request_start
      && seq < completion_seq
      && entry.envelope.meta.turn_id == introducing.envelope.meta.turn_id
      && entry.envelope.meta.model_epoch == introducing.envelope.meta.model_epoch
      && matches!(entry.envelope.event, AgentEvent::AssistantDelta(_))
  }) {
    let event = restore_externalized_event_from_blobs(entry, blobs, session)?;
    let AgentEvent::AssistantDelta(delta) = event else {
      unreachable!("assistant delta filter only matches assistant deltas")
    };
    expected.push_str(&delta.text);
  }
  if message.message.text() != expected {
    return Err(StoreError::Invalid(format!(
      "session {session} assistant message {} content disagrees with canonical deltas; resume requires recovery",
      message.event_id
    )));
  }
  Ok(())
}

fn validate_message_projection(
  message: &SessionMessage,
  trace_entry: &rupi_core::TraceEntry,
  blobs: &BlobStore,
  session: &SessionId,
) -> Result<(), StoreError> {
  let meta = &trace_entry.envelope.meta;
  let restored_event = if trace_entry.externalized.is_empty() {
    None
  } else {
    Some(restore_externalized_event_from_blobs(
      trace_entry,
      blobs,
      session,
    )?)
  };
  let event = restored_event
    .as_ref()
    .unwrap_or(&trace_entry.envelope.event);
  if meta.turn_id.as_ref() != Some(&message.turn_id)
    || meta.model_epoch.is_some_and(|epoch| epoch != message.epoch)
    || meta
      .model
      .as_ref()
      .is_some_and(|model| model != &message.model)
  {
    return Err(StoreError::Invalid(format!(
      "session {session} semantic event {} attribution disagrees with its canonical trace; resume requires recovery",
      message.event_id
    )));
  }
  let invalid = |detail: &str| {
    StoreError::Invalid(format!(
      "session {session} semantic event {} has an invalid message projection ({detail}); resume requires recovery",
      message.event_id
    ))
  };
  match event {
    AgentEvent::UserMessage(user) => {
      // Low-level callers may use a user event as the causal introducer for a
      // richer assistant message (the provenance round-trip API does this).
      // Only a semantic user projection has the user text/attachment shape to
      // validate here.
      if message.role == Role::User {
        let image_count = message
          .message
          .content
          .iter()
          .filter(|block| matches!(block, rupi_core::ContentBlock::Image { .. }))
          .count();
        let valid = message.message.content.iter().all(|block| {
          matches!(
            block,
            rupi_core::ContentBlock::Text { .. } | rupi_core::ContentBlock::Image { .. }
          )
        }) && message.message.text() == user.text
          // `attachments` also counts imported block kinds that rupi cannot
          // preserve; every retained image must still be accounted for.
          && u32::try_from(image_count).ok().is_some_and(|count| count <= user.attachments);
        if !valid {
          return Err(invalid("user text, role, or attachment mismatch"));
        }
      }
    }
    AgentEvent::ExternalContextRetrieved(retrieved) => {
      if message.role != Role::User
        || message.message.content.len() != 1
        || !matches!(
          message.message.content.first(),
          Some(rupi_core::ContentBlock::Text { .. })
        )
      {
        return Err(invalid("external context is not a user message"));
      }
      let Some(context) = message.external_context.as_ref() else {
        return Err(invalid("external context reference is missing"));
      };
      if context.provider != retrieved.source.provider
        || context.resource_id != retrieved.source.resource_id
        || context.citation != retrieved.citation
        || context.provenance != retrieved.source.provenance
        || context.metadata != retrieved.metadata
      {
        return Err(invalid("external context attribution mismatch"));
      }
    }
    AgentEvent::ContextSummary => {
      if message.role != Role::User
        || message.message.content.len() != 1
        || !matches!(
          message.message.content.first(),
          Some(rupi_core::ContentBlock::Text { .. })
        )
      {
        return Err(invalid("summary is not a user message"));
      }
    }
    AgentEvent::AssistantDelta(_) => {
      if message.role != Role::Assistant {
        return Err(invalid("assistant delta is not an assistant message"));
      }
    }
    AgentEvent::ModelRequestCompleted(completed) => {
      if message.role != Role::Assistant {
        return Err(invalid("model completion is not an assistant message"));
      }
      let tool_calls = message
        .message
        .content
        .iter()
        .filter(|block| matches!(block, rupi_core::ContentBlock::ToolCall(_)))
        .count();
      if tool_calls != completed.tool_calls as usize {
        return Err(invalid("tool-call count mismatch"));
      }
    }
    AgentEvent::ToolCompleted(completed) => {
      validate_tool_result_projection(
        message,
        completed.call_id.clone(),
        completed.name.as_str(),
        ToolExecutionState::Succeeded,
        false,
        &invalid,
      )?;
      let rupi_core::ContentBlock::ToolResult(result) = &message.message.content[0] else {
        unreachable!("tool result validation proved the block shape")
      };
      if result.reduced != completed.reduced || result.text.len() as u64 != completed.visible_bytes
      {
        return Err(invalid(
          "tool result reduction or visible-byte metadata mismatch",
        ));
      }
    }
    AgentEvent::ToolFailed(failed) => {
      validate_tool_result_projection(
        message,
        failed.call_id.clone(),
        failed.name.as_str(),
        ToolExecutionState::Failed,
        true,
        &invalid,
      )?;
    }
    AgentEvent::ToolUnknown(unknown) => {
      validate_tool_result_projection(
        message,
        unknown.call_id.clone(),
        unknown.name.as_str(),
        ToolExecutionState::Unknown,
        true,
        &invalid,
      )?;
    }
    _ => return Err(invalid("event does not introduce a message")),
  }
  Ok(())
}

fn validate_tool_result_projection(
  message: &SessionMessage,
  call_id: ToolCallId,
  name: &str,
  state: ToolExecutionState,
  expected_error: bool,
  invalid: &impl Fn(&str) -> StoreError,
) -> Result<(), StoreError> {
  if message.role != Role::Tool || message.message.content.len() != 1 {
    return Err(invalid("tool result role or shape mismatch"));
  }
  let rupi_core::ContentBlock::ToolResult(result) = &message.message.content[0] else {
    return Err(invalid("tool result block is missing"));
  };
  let state_matches = match state {
    ToolExecutionState::Failed => {
      matches!(
        result.state,
        ToolExecutionState::Failed | ToolExecutionState::Requested
      )
    }
    ToolExecutionState::Unknown => matches!(
      result.state,
      ToolExecutionState::Unknown | ToolExecutionState::Started | ToolExecutionState::Requested
    ),
    _ => result.state == state,
  };
  if result.id != call_id
    || result.name != name
    || !state_matches
    || (expected_error && !result.is_error)
  {
    return Err(invalid("tool result metadata mismatch"));
  }
  Ok(())
}

/// Return requests that never crossed the durable `ToolStarted` boundary.
/// Lifecycle shape is checked here before normalization; the full validator
/// runs again after the synthetic terminal records are appended.
fn unstarted_tool_requests(
  entries: &[rupi_core::TraceEntry],
  _session: &SessionId,
) -> Result<Vec<rupi_core::TraceEntry>, StoreError> {
  let pending = scan_tool_lifecycles(entries)?;
  Ok(
    pending
      .into_iter()
      .filter_map(|call| (!call.started).then_some(call.request))
      .collect(),
  )
}

#[derive(Debug, Clone)]
struct PendingToolLifecycle {
  request: rupi_core::TraceEntry,
  started: bool,
  started_event_id: Option<EventId>,
}

/// Resolve a lifecycle edge by its causal parent when present. Parentless
/// entries are an intentionally supported legacy format, so they fall back to
/// the call id only when exactly one active request can match it.
fn resolve_pending_tool(
  pending: &BTreeMap<EventId, PendingToolLifecycle>,
  entry: &rupi_core::TraceEntry,
  call_id: &ToolCallId,
) -> Result<EventId, &'static str> {
  if let Some(parent) = entry.envelope.meta.parent_event_id.as_ref() {
    if pending.contains_key(parent) {
      return Ok(parent.clone());
    }
    if let Some((event_id, _)) = pending
      .iter()
      .find(|(_, call)| call.started_event_id.as_ref() == Some(parent))
    {
      return Ok(event_id.clone());
    }
    return Err("tool lifecycle parent does not identify a pending request");
  }

  let mut matches = pending.iter().filter(|(_, call)| {
    matches!(
      &call.request.envelope.event,
      AgentEvent::ToolRequested(requested) if requested.call_id == *call_id
    )
  });
  let Some((event_id, _)) = matches.next() else {
    return Err("tool lifecycle has no matching request");
  };
  if matches.next().is_some() {
    return Err("parentless tool lifecycle is ambiguous for this call id");
  }
  Ok(event_id.clone())
}

fn tool_call_metadata_matches(entry: &rupi_core::TraceEntry, call_id: &ToolCallId) -> bool {
  entry
    .envelope
    .meta
    .tool_call_id
    .as_ref()
    .is_none_or(|metadata_id| metadata_id == call_id)
}

fn lifecycle_invalid(entry: &rupi_core::TraceEntry, detail: &str) -> StoreError {
  StoreError::Invalid(format!(
    "session {} has an invalid tool lifecycle at {}: {detail}; resume requires recovery",
    entry.envelope.meta.session_id, entry.envelope.meta.event_id
  ))
}

fn scan_tool_lifecycles(
  entries: &[rupi_core::TraceEntry],
) -> Result<Vec<PendingToolLifecycle>, StoreError> {
  let mut pending = BTreeMap::<EventId, PendingToolLifecycle>::new();
  let mut ordered = Vec::<EventId>::new();

  for entry in entries {
    match &entry.envelope.event {
      AgentEvent::ToolRequested(requested) => {
        let invalid = |detail: &str| lifecycle_invalid(entry, detail);
        if entry.envelope.meta.turn_id.is_none()
          || requested.call_id.as_str().is_empty()
          || !tool_call_metadata_matches(entry, &requested.call_id)
        {
          return Err(invalid("tool request has no turn or call identity"));
        }
        let event_id = entry.envelope.meta.event_id.clone();
        if pending
          .insert(
            event_id.clone(),
            PendingToolLifecycle {
              request: entry.clone(),
              started: false,
              started_event_id: None,
            },
          )
          .is_some()
        {
          return Err(invalid("duplicate tool request event identity"));
        }
        ordered.push(event_id);
      }
      AgentEvent::ToolStarted(started) => {
        let request_id = resolve_pending_tool(&pending, entry, &started.call_id)
          .map_err(|detail| lifecycle_invalid(entry, detail))?;
        let call = pending
          .get_mut(&request_id)
          .expect("resolved pending tool must remain in map");
        if call.started {
          return Err(lifecycle_invalid(entry, "duplicate tool start"));
        }
        let AgentEvent::ToolRequested(requested) = &call.request.envelope.event else {
          unreachable!("pending tool entries are requests");
        };
        if entry
          .envelope
          .meta
          .parent_event_id
          .as_ref()
          .is_some_and(|parent| parent != &request_id)
          || started.call_id != requested.call_id
          || started.name != requested.name
          || !tool_call_metadata_matches(entry, &started.call_id)
          || call.request.envelope.meta.turn_id != entry.envelope.meta.turn_id
          || call.request.envelope.meta.model_epoch != entry.envelope.meta.model_epoch
          || call.request.envelope.meta.model != entry.envelope.meta.model
        {
          return Err(lifecycle_invalid(
            entry,
            "tool start disagrees with its request",
          ));
        }
        call.started = true;
        call.started_event_id = Some(entry.envelope.meta.event_id.clone());
      }
      AgentEvent::ToolCompleted(completed) => {
        let request_id = resolve_pending_tool(&pending, entry, &completed.call_id)
          .map_err(|detail| lifecycle_invalid(entry, detail))?;
        let call = pending
          .remove(&request_id)
          .expect("resolved pending tool must remain in map");
        let AgentEvent::ToolRequested(requested) = &call.request.envelope.event else {
          unreachable!("pending tool entries are requests");
        };
        if !call.started {
          return Err(lifecycle_invalid(entry, "tool completed before it started"));
        }
        if completed.call_id != requested.call_id
          || completed.name != requested.name
          || !tool_call_metadata_matches(entry, &completed.call_id)
          || call.request.envelope.meta.turn_id != entry.envelope.meta.turn_id
          || call.request.envelope.meta.model_epoch != entry.envelope.meta.model_epoch
          || call.request.envelope.meta.model != entry.envelope.meta.model
        {
          return Err(lifecycle_invalid(
            entry,
            "tool completion disagrees with its request",
          ));
        }
        if completed.state != ToolExecutionState::Succeeded {
          return Err(lifecycle_invalid(
            entry,
            "tool completed with a non-success state",
          ));
        }
      }
      AgentEvent::ToolFailed(failed) => {
        let request_id = resolve_pending_tool(&pending, entry, &failed.call_id)
          .map_err(|detail| lifecycle_invalid(entry, detail))?;
        let call = pending
          .remove(&request_id)
          .expect("resolved pending tool must remain in map");
        let AgentEvent::ToolRequested(requested) = &call.request.envelope.event else {
          unreachable!("pending tool entries are requests");
        };
        // A failed request may be recorded before execution begins when
        // cancellation or provider failure prevents the runtime from emitting
        // ToolStarted. That is a safe terminal state, not an interrupted
        // mutating operation.
        if failed.call_id != requested.call_id
          || failed.name != requested.name
          || !tool_call_metadata_matches(entry, &failed.call_id)
          || call.request.envelope.meta.turn_id != entry.envelope.meta.turn_id
          || call.request.envelope.meta.model_epoch != entry.envelope.meta.model_epoch
          || call.request.envelope.meta.model != entry.envelope.meta.model
        {
          return Err(lifecycle_invalid(
            entry,
            "tool failure disagrees with its request",
          ));
        }
      }
      AgentEvent::ToolUnknown(unknown) => {
        let request_id = resolve_pending_tool(&pending, entry, &unknown.call_id)
          .map_err(|detail| lifecycle_invalid(entry, detail))?;
        let call = pending
          .remove(&request_id)
          .expect("resolved pending tool must remain in map");
        let AgentEvent::ToolRequested(requested) = &call.request.envelope.event else {
          unreachable!("pending tool entries are requests");
        };
        if unknown.mutating == requested.read_only
          || unknown.call_id != requested.call_id
          || unknown.name != requested.name
          || !tool_call_metadata_matches(entry, &unknown.call_id)
          || call.request.envelope.meta.turn_id != entry.envelope.meta.turn_id
          || call.request.envelope.meta.model_epoch != entry.envelope.meta.model_epoch
          || call.request.envelope.meta.model != entry.envelope.meta.model
        {
          return Err(lifecycle_invalid(
            entry,
            "tool unknown result disagrees with its request",
          ));
        }
      }
      _ => {}
    }
  }

  // Keep the original request order for deterministic recovery.
  Ok(
    ordered
      .into_iter()
      .filter_map(|event_id| pending.remove(&event_id))
      .collect(),
  )
}

fn interrupted_tool_calls(
  entries: &[rupi_core::TraceEntry],
) -> Result<Vec<InterruptedToolCall>, StoreError> {
  let pending = scan_tool_lifecycles(entries)?;
  let mut interrupted = Vec::new();
  for call in pending {
    if !call.started {
      let call_id = match &call.request.envelope.event {
        AgentEvent::ToolRequested(requested) => requested.call_id.clone(),
        _ => unreachable!("pending tool entries are requests"),
      };
      return Err(StoreError::Invalid(format!(
        "durable tool request {call_id} has no ToolStarted; resume requires recovery"
      )));
    }
    let AgentEvent::ToolRequested(requested) = &call.request.envelope.event else {
      unreachable!("pending tool entries are requests");
    };
    interrupted.push(InterruptedToolCall {
      request: ToolRequest {
        call_id: requested.call_id.clone(),
        name: requested.name.clone(),
        arguments: requested.arguments.clone(),
      },
      state: ToolExecutionState::Started,
      read_only: requested.read_only,
      turn_id: call.request.envelope.meta.turn_id.clone(),
      epoch: call.request.envelope.meta.model_epoch,
      model: call.request.envelope.meta.model.clone(),
      request_event_id: Some(call.request.envelope.meta.event_id.clone()),
      started_event_id: call.started_event_id,
    });
  }
  Ok(interrupted)
}

fn stamp_projection_seq(record: &mut SessionRecord, seq: EventSeq) {
  if let SessionRecord::Reduction(reduction) = record {
    reduction.seq = Some(seq);
  }
}

fn has_projection_for(record: &SessionRecord, records: &[SessionRecord]) -> bool {
  records.iter().any(|candidate| match (candidate, record) {
    (SessionRecord::Message(candidate), SessionRecord::Message(record)) => {
      candidate.event_id == record.event_id
    }
    (SessionRecord::Epoch(candidate), SessionRecord::Epoch(record)) => {
      candidate.epoch == record.epoch
        && candidate.model == record.model
        && candidate.reason == record.reason
    }
    (SessionRecord::Compaction(candidate), SessionRecord::Compaction(record)) => {
      candidate.context_epoch == record.context_epoch
        && candidate.level == record.level
        && candidate.removed_messages == record.removed_messages
        && candidate.retained_messages == record.retained_messages
        && candidate.summary_present == record.summary_present
        && candidate.replaces_from == record.replaces_from
        && candidate.replaces_through == record.replaces_through
    }
    (SessionRecord::CheckpointBarrier(candidate), SessionRecord::CheckpointBarrier(record)) => {
      candidate.checkpoint_id == record.checkpoint_id
        && candidate.capsule_version == record.capsule_version
        && candidate.context_epoch == record.context_epoch
        && candidate.capsule_path == record.capsule_path
    }
    (SessionRecord::Reduction(candidate), SessionRecord::Reduction(record)) => {
      candidate.event_id == record.event_id
    }
    _ => false,
  })
}

fn restore_externalized_event(
  trace_entry: &rupi_core::TraceEntry,
  session: &Session,
) -> Result<AgentEvent, StoreError> {
  restore_externalized_event_from_blobs(trace_entry, session.blobs(), session.id())
}

fn restore_externalized_event_from_blobs(
  trace_entry: &rupi_core::TraceEntry,
  blobs: &BlobStore,
  session: &SessionId,
) -> Result<AgentEvent, StoreError> {
  if trace_entry.externalized.is_empty() {
    return Ok(trace_entry.envelope.event.clone());
  }
  let mut value = serde_json::to_value(&trace_entry.envelope.event)?;
  let mut fields = BTreeSet::new();
  for field in &trace_entry.externalized {
    if field.bytes == 0 || !fields.insert(field.field.clone()) {
      return Err(StoreError::Invalid(format!(
        "session {session} has an invalid or duplicate externalized trace field {}",
        field.field
      )));
    }
    let bytes = blobs
      .get_relative_verified(&field.reference)
      .map_err(|error| {
        StoreError::Invalid(format!(
          "session {session} cannot recover externalized trace field {}: {error}",
          field.field
        ))
      })?;
    if bytes.len() as u64 != field.bytes {
      return Err(StoreError::Invalid(format!(
        "session {session} externalized trace field {} declares {} bytes but blob has {}",
        field.field,
        field.bytes,
        bytes.len()
      )));
    }
    let text = String::from_utf8(bytes).map_err(|_| {
      StoreError::Invalid(format!(
        "session {session} externalized trace field {} is not UTF-8",
        field.field
      ))
    })?;
    if !replace_string_field(&mut value, &field.field, text) {
      return Err(StoreError::Invalid(format!(
        "session {session} externalized trace field {} does not exist in its event",
        field.field
      )));
    }
  }
  serde_json::from_value(value).map_err(|error| {
    StoreError::Invalid(format!(
      "session {session} externalized trace event cannot be decoded: {error}"
    ))
  })
}

fn replace_string_field(value: &mut serde_json::Value, path: &str, text: String) -> bool {
  let segments: Vec<&str> = path
    .split('/')
    .filter(|segment| !segment.is_empty())
    .collect();
  if segments.is_empty() {
    return false;
  }
  replace_string_segments(value, &segments, text)
}

fn replace_string_segments(value: &mut serde_json::Value, segments: &[&str], text: String) -> bool {
  let Some((head, tail)) = segments.split_first() else {
    return false;
  };
  if tail.is_empty() {
    let target = match value {
      serde_json::Value::Object(map) => map.get_mut(*head),
      serde_json::Value::Array(items) => head
        .parse::<usize>()
        .ok()
        .and_then(|index| items.get_mut(index)),
      _ => None,
    };
    let Some(target) = target else {
      return false;
    };
    if !target.is_string() {
      return false;
    }
    *target = serde_json::Value::String(text);
    return true;
  }
  let child = match value {
    serde_json::Value::Object(map) => map.get_mut(*head),
    serde_json::Value::Array(items) => head
      .parse::<usize>()
      .ok()
      .and_then(|index| items.get_mut(index)),
    _ => None,
  };
  child.is_some_and(|child| replace_string_segments(child, tail, text))
}

fn validate_session_record_size(
  record: &SessionRecord,
  redaction: &RedactionPolicy,
) -> Result<(), StoreError> {
  let mut value = serde_json::to_value(record)?;
  redaction.apply_json(&mut value);
  let bytes = serde_json::to_vec(&value)?.len();
  if bytes.saturating_add(1) > crate::jsonl::MAX_JSONL_LINE_BYTES {
    return Err(StoreError::Invalid(format!(
      "session record exceeds the {}-byte JSONL line bound",
      crate::jsonl::MAX_JSONL_LINE_BYTES
    )));
  }
  Ok(())
}

fn validate_message_envelope(envelope: &EventEnvelope) -> Result<(), StoreError> {
  if envelope.meta.turn_id.is_none() {
    return Err(StoreError::Invalid(
      "a persisted message must belong to a turn".into(),
    ));
  }
  if envelope.meta.model_epoch.is_none() {
    return Err(StoreError::Invalid(
      "a persisted message must carry a model epoch".into(),
    ));
  }
  if envelope.meta.model.is_none() {
    return Err(StoreError::Invalid(
      "a persisted message must carry a model".into(),
    ));
  }
  Ok(())
}

fn recover_projection_record(
  trace_entry: &rupi_core::TraceEntry,
  trace: &[rupi_core::TraceEntry],
  records: &[SessionRecord],
  session: &Session,
  recovery: Option<&MessageRecovery>,
) -> Result<Option<SessionRecord>, StoreError> {
  let seq = trace_entry.envelope.meta.seq;
  let meta = &trace_entry.envelope.meta;
  let message_attribution = || {
    let turn_id = meta
      .turn_id
      .clone()
      .ok_or_else(|| StoreError::Invalid("cannot recover a message without a turn id".into()))?;
    let epoch = meta.model_epoch.ok_or_else(|| {
      StoreError::Invalid("cannot recover a message without a model epoch".into())
    })?;
    let model = meta
      .model
      .clone()
      .ok_or_else(|| StoreError::Invalid("cannot recover a message without a model".into()))?;
    Ok::<_, StoreError>((turn_id, epoch, model))
  };
  if let Some(recovery) = recovery {
    let message = session.recover_message(recovery)?;
    let seq = seq.ok_or_else(|| {
      StoreError::Invalid(format!(
        "session {} message event {} has no canonical sequence",
        session.id(),
        trace_entry.envelope.meta.event_id
      ))
    })?;
    return Ok(Some(session.message_record(
      &trace_entry.envelope,
      &message,
      seq,
    )?));
  }
  match &trace_entry.envelope.event {
    AgentEvent::UserMessage(_) => {
      let event = restore_externalized_event(trace_entry, session)?;
      let AgentEvent::UserMessage(user) = event else {
        return Err(StoreError::Invalid(format!(
          "session {} externalized user event changed type during recovery",
          session.id()
        )));
      };
      let (turn_id, epoch, model) = message_attribution()?;
      Ok(Some(SessionRecord::Message(SessionMessage {
        turn_id,
        role: Role::User,
        message: Message::user(user.text),
        epoch,
        model,
        event_id: trace_entry.envelope.meta.event_id.clone(),
        seq,
        external_context: None,
      })))
    }
    AgentEvent::ExternalContextRetrieved(_) => Err(StoreError::Invalid(format!(
      "session {} has an interrupted external-context message that cannot be reconstructed safely",
      session.id()
    ))),
    AgentEvent::ContextSummary | AgentEvent::ToolCompleted(_) => Err(StoreError::Invalid(format!(
      "session {} has an interrupted message projection for {}; resume requires manual recovery",
      session.id(),
      trace_entry.envelope.meta.event_id
    ))),
    AgentEvent::ToolFailed(failed) => {
      let (turn_id, epoch, model) = message_attribution()?;
      Ok(Some(SessionRecord::Message(SessionMessage {
        turn_id,
        role: Role::Tool,
        message: Message::new(
          Role::Tool,
          vec![ContentBlock::ToolResult(ToolResultBlock {
            id: failed.call_id.clone(),
            name: failed.name.clone(),
            state: ToolExecutionState::Failed,
            text: failed.message.clone(),
            is_error: true,
            reduced: false,
          })],
        ),
        epoch,
        model,
        event_id: trace_entry.envelope.meta.event_id.clone(),
        seq,
        external_context: None,
      })))
    }
    AgentEvent::ToolUnknown(unknown) => {
      let (turn_id, epoch, model) = message_attribution()?;
      Ok(Some(SessionRecord::Message(SessionMessage {
        turn_id,
        role: Role::Tool,
        message: Message::new(
          Role::Tool,
          vec![ContentBlock::ToolResult(ToolResultBlock {
            id: unknown.call_id.clone(),
            name: unknown.name.clone(),
            state: ToolExecutionState::Unknown,
            text: unknown.why.clone(),
            is_error: true,
            reduced: false,
          })],
        ),
        epoch,
        model,
        event_id: trace_entry.envelope.meta.event_id.clone(),
        seq,
        external_context: None,
      })))
    }
    AgentEvent::ModelRequestCompleted(completed) => {
      if completed.finish_reason.as_deref() == Some("abandoned") {
        return Ok(None);
      }
      if completed.finish_reason.is_none() {
        // Failed requests do not enter assistant history. Their tool failures,
        // when any, carry independent intents and will be reconciled separately.
        return Ok(None);
      }
      if completed.tool_calls > 0 {
        // Tool-call arguments are emitted in `ToolRequested`, after this
        // completion and after the assistant message is normally projected.
        // There is no safe way to recreate the assistant's tool-call blocks
        // from the completion aggregate alone, so continuation must stop rather
        // than feed an incomplete protocol history to a provider.
        return Err(StoreError::Invalid(format!(
          "session {} has an interrupted assistant tool-call projection for {}; resume requires manual recovery",
          session.id(),
          trace_entry.envelope.meta.event_id
        )));
      }
      let (turn_id, epoch, model) = message_attribution()?;
      let completion_seq = seq.ok_or_else(|| {
        StoreError::Invalid(
          "cannot recover an assistant message without a canonical sequence".into(),
        )
      })?;
      // A turn can issue several requests (retries, overflow recovery, or tool
      // rounds) under one epoch. The nearest preceding request boundary is the
      // only safe range for this completion; grouping by turn/epoch alone would
      // merge deltas from an earlier attempt into the resumed answer.
      let request_start = trace
        .iter()
        .filter_map(|candidate| {
          let candidate_seq = candidate.envelope.meta.seq?;
          if candidate_seq >= completion_seq
            || candidate.envelope.meta.turn_id.as_ref() != Some(&turn_id)
            || candidate.envelope.meta.model_epoch != Some(epoch)
          {
            return None;
          }
          matches!(candidate.envelope.event, AgentEvent::ModelRequestStarted(_))
            .then_some(candidate_seq)
        })
        .max()
        .ok_or_else(|| {
          StoreError::Invalid(format!(
            "session {} has an assistant completion without a request boundary; resume requires manual recovery",
            session.id()
          ))
        })?;
      let mut text = String::new();
      let mut first = None;
      for candidate in trace.iter().filter(|candidate| {
        let Some(candidate_seq) = candidate.envelope.meta.seq else {
          return false;
        };
        candidate_seq > request_start
          && candidate_seq < completion_seq
          && candidate.envelope.meta.turn_id.as_ref() == Some(&turn_id)
          && candidate.envelope.meta.model_epoch == Some(epoch)
          && matches!(candidate.envelope.event, AgentEvent::AssistantDelta(_))
      }) {
        let event = restore_externalized_event(candidate, session)?;
        let AgentEvent::AssistantDelta(delta) = event else {
          return Err(StoreError::Invalid(format!(
            "session {} externalized assistant event changed type during recovery",
            session.id()
          )));
        };
        if !delta.text.is_empty() {
          first.get_or_insert(candidate);
          text.push_str(&delta.text);
        }
      }
      if text.is_empty() {
        return Ok(None);
      }
      let first = first.expect("a non-empty delta establishes the message event");
      Ok(Some(SessionRecord::Message(SessionMessage {
        turn_id,
        role: Role::Assistant,
        message: Message::assistant(text),
        epoch,
        model,
        event_id: first.envelope.meta.event_id.clone(),
        seq: first.envelope.meta.seq,
        external_context: None,
      })))
    }
    AgentEvent::ModelEpochStarted(started) => {
      Ok(Some(SessionRecord::Epoch(rupi_core::SessionEpochRecord {
        epoch: started.epoch,
        model: started.model.clone(),
        reason: started.reason.clone(),
      })))
    }
    AgentEvent::ContextCompactionCompleted(completed) => {
      let (summary_present, range) = if completed.level == rupi_core::ContextLevel::L3Checkpoint {
        // Checkpoints publish their barrier through `CheckpointCreated`; the
        // completion only closes that already durable capsule transition.
        (false, None)
      } else {
        let completion_seq = seq.ok_or_else(|| {
          StoreError::Invalid(format!(
            "session {} has a compaction completion without a canonical sequence",
            session.id()
          ))
        })?;
        let epochs: Vec<_> = trace
          .iter()
          .filter_map(|candidate| {
            let candidate_seq = candidate.envelope.meta.seq?;
            if candidate_seq >= completion_seq
              || candidate.envelope.meta.turn_id != trace_entry.envelope.meta.turn_id
            {
              return None;
            }
            match &candidate.envelope.event {
              AgentEvent::ContextCompactionEpoch(epoch)
                if epoch.context_epoch == completed.context_epoch =>
              {
                Some((candidate_seq, epoch))
              }
              _ => None,
            }
          })
          .collect();
        let Some((_, epoch)) = epochs.as_slice().first() else {
          return Err(StoreError::Invalid(format!(
            "session {} compaction epoch {} is missing from the canonical trace; resume requires recovery",
            session.id(),
            completed.context_epoch
          )));
        };
        if epochs.len() != 1 {
          return Err(StoreError::Invalid(format!(
            "session {} compaction epoch {} is ambiguous in the canonical trace; resume requires recovery",
            session.id(),
            completed.context_epoch
          )));
        }
        let summary_entries: Vec<_> = trace
          .iter()
          .filter(|candidate| {
            let Some(candidate_seq) = candidate.envelope.meta.seq else {
              return false;
            };
            candidate_seq < completion_seq
              && candidate_seq > epoch.replaces_through
              && candidate.envelope.meta.turn_id == trace_entry.envelope.meta.turn_id
              && matches!(candidate.envelope.event, AgentEvent::ContextSummary)
          })
          .collect();
        let Some(summary_entry) = summary_entries.as_slice().first() else {
          return Err(StoreError::Invalid(format!(
            "session {} compaction epoch {} has no canonical summary event; resume requires recovery",
            session.id(),
            completed.context_epoch
          )));
        };
        if summary_entries.len() != 1 {
          return Err(StoreError::Invalid(format!(
            "session {} compaction epoch {} has an ambiguous canonical summary; resume requires recovery",
            session.id(),
            completed.context_epoch
          )));
        }
        if !records.iter().any(|record| {
          matches!(
            record,
            SessionRecord::Message(message)
              if message.event_id == summary_entry.envelope.meta.event_id
          )
        }) {
          return Err(StoreError::Invalid(format!(
            "session {} compaction epoch {} has no durable summary projection; resume requires recovery",
            session.id(),
            completed.context_epoch
          )));
        }
        (true, Some((epoch.replaces_from, epoch.replaces_through)))
      };
      Ok(Some(SessionRecord::Compaction(
        rupi_core::SessionCompactionRecord {
          context_epoch: completed.context_epoch,
          level: completed.level,
          removed_messages: completed.removed_messages,
          retained_from: 0,
          retained_messages: completed.retained_messages,
          summary_present,
          replaces_from: range.map(|(from, _)| from),
          replaces_through: range.map(|(_, through)| through),
          aborted: false,
          start_event_id: None,
          summary_event_id: None,
        },
      )))
    }
    AgentEvent::ContextReduced(reduced) if reduced.removed_messages > 0 => Ok(Some(
      SessionRecord::Reduction(rupi_core::SessionReductionRecord {
        event_id: trace_entry.envelope.meta.event_id.clone(),
        seq,
        reason: reduced.reason.clone(),
        removed_messages: reduced.removed_messages,
        retained_messages: reduced.retained_messages,
      }),
    )),
    AgentEvent::CheckpointCreated(created) => {
      let path = session
        .layout
        .checkpoint_path(session.id(), &created.checkpoint_id);
      let capsule = read_checkpoint_capsule(&path, session.id(), &created.checkpoint_id)?;
      Ok(Some(SessionRecord::CheckpointBarrier(
        SessionCheckpointRecord {
          checkpoint_id: created.checkpoint_id.clone(),
          capsule_version: created.capsule_version,
          context_epoch: created.context_epoch,
          capsule_path: created.path.clone(),
          capsule,
        },
      )))
    }
    _ => Ok(None),
  }
}

/// Redact one durable payload while preserving its original bytes when no change is needed.
///
/// Provider and tool payloads are commonly JSON, so structured values must use
/// `apply_json` rather than only scanning the serialized text: a credential under
/// `api_key` or `token` is sensitive even when it has no recognizable prefix.
/// Non-JSON text uses the existing UTF-8-lossy fallback; unchanged binary bytes
/// remain byte-for-byte recoverable.
fn redact_payload(policy: &RedactionPolicy, bytes: &[u8]) -> Result<Vec<u8>, StoreError> {
  if let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(bytes) {
    if policy.apply_json(&mut value) == 0 {
      return Ok(bytes.to_vec());
    }
    return Ok(serde_json::to_vec(&value)?);
  }
  let redacted = policy.apply(&String::from_utf8_lossy(bytes));
  if redacted.replacements == 0 {
    Ok(bytes.to_vec())
  } else {
    Ok(redacted.text.into_bytes())
  }
}

#[cfg(test)]
mod tests {
  use rupi_core::{
    capability::EpochReason,
    context::{CAPSULE_SCHEMA_VERSION, ReductionReason},
    event::{
      AgentEvent, CheckpointCreated, ContextReduced, Diagnostic, DiagnosticLevel, EventMeta,
      ModelRequestCompleted, ModelRequestStarted, SessionEndReason, SessionEnded, SessionStarted,
      ToolCompleted, ToolRequested, ToolStarted, TurnCompleted, TurnStatus, UserMessage,
    },
    ids::{EventId, ToolCallId, TraceId, uuidv7},
    message::ToolCallBlock,
    session::{SESSION_SCHEMA_VERSION, SessionEpochRecord, SessionReductionRecord},
    trace::{BlobCompression, TraceRetention},
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
      span_id: rupi_core::ids::SpanId::new(),
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
        call_id: ToolCallId::from_string("77777777-7777-4777-8777-777777777777"),
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
    let mut requested = huge_write(&id, &turn, &contents);
    session.emit(&mut requested).unwrap();
    // Close the synthetic request without a start: this test exercises payload
    // bounding, not an interrupted tool lifecycle.
    session
      .emit(&mut EventEnvelope::new(
        meta(&id, &turn),
        AgentEvent::ToolFailed(rupi_core::ToolFailed {
          call_id: ToolCallId::from_string("77777777-7777-4777-8777-777777777777"),
          name: "write".into(),
          message: "synthetic fixture did not execute".into(),
          duration_ms: 0,
          status: None,
        }),
      ))
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
        &rupi_core::message::Message::user("write the file"),
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
      Some(EventSeq(3)),
      "order survives bounding"
    );
    let report = TraceJournal::read(resumed.trace_path()).unwrap();
    assert_eq!(report.items.len(), 3);
    assert_eq!(report.items[0].externalized.len(), 1);
    assert_eq!(
      report.items[2].externalized.len(),
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
        &rupi_core::message::Message::user("hello"),
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
        &rupi_core::message::Message::user("hello"),
        0,
        &ModelRef::new("local", "qwen"),
        &introduced,
      )
      .unwrap();
    assert_eq!(record.seq, Some(EventSeq(1)));
    assert_eq!(record.event_id, introduced.meta.event_id);
    assert_eq!(record.role, rupi_core::message::Role::User);
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
            &rupi_core::message::Message::user(format!("long message {index}")),
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
          context_epoch: barrier.context_epoch,
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
          &rupi_core::message::Message::user("after"),
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
  fn message_transaction_recovers_or_rolls_back_at_each_boundary() {
    let cases = [
      (ProjectionFailpoint::Prepare, false),
      (ProjectionFailpoint::CanonicalAppend, true),
      (ProjectionFailpoint::SemanticAppend, true),
    ];
    for (failpoint, should_restore) in cases {
      let tmp = TempDir::new(&format!("message-transaction-{failpoint:?}"));
      let opened = store(&tmp);
      let id = SessionId::from_string(uuidv7());
      let turn = TurnId::new();
      let mut session = opened.begin(header(&id)).unwrap();
      let mut envelope = EventEnvelope::new(
        meta(&id, &turn),
        AgentEvent::UserMessage(UserMessage {
          text: "exact durable message".into(),
          attachments: 0,
        }),
      );
      session.set_failpoint(failpoint);
      assert!(
        session
          .emit_message(&mut envelope, &Message::user("exact durable message"))
          .is_err(),
        "the synthetic interruption must fire at {failpoint:?}"
      );
      drop(session);

      assert!(
        matches!(opened.restore(&id), Err(StoreError::Invalid(message)) if message.contains("incomplete trace/session projection")),
        "read-only restore must not consume {failpoint:?}"
      );
      let resumed = opened.resume(&id).unwrap();
      let restored = opened.restore(&id).unwrap();
      if should_restore {
        assert_eq!(restored.messages.len(), 1, "{failpoint:?}");
        assert_eq!(
          restored.messages[0].message,
          Message::user("exact durable message")
        );
        assert_eq!(restored.messages[0].seq, Some(EventSeq(1)));
      } else {
        assert!(
          restored.messages.is_empty(),
          "{failpoint:?} rolls back safely"
        );
        assert_eq!(restored.last_seq, None);
      }
      assert!(
        ProjectionWal::pending_at(&opened.layout().wal_path(&id))
          .unwrap()
          .is_empty()
      );
      resumed.finish().unwrap();
    }
  }

  #[test]
  fn message_failpoints_cover_every_runtime_message_shape() {
    #[derive(Debug, Clone, Copy)]
    enum Case {
      User,
      ExternalContext,
      AssistantText,
      AssistantCalls,
      ToolCompleted,
      ReducedToolCompleted,
      ToolFailed,
      ToolUnknown,
    }

    let cases = [
      Case::User,
      Case::ExternalContext,
      Case::AssistantText,
      Case::AssistantCalls,
      Case::ToolCompleted,
      Case::ReducedToolCompleted,
      Case::ToolFailed,
      Case::ToolUnknown,
    ];
    let failpoints = [
      ProjectionFailpoint::Prepare,
      ProjectionFailpoint::CanonicalAppend,
      ProjectionFailpoint::SemanticAppend,
    ];

    for case in cases {
      for failpoint in failpoints {
        let tmp = TempDir::new(&format!("message-shape-{case:?}-{failpoint:?}"));
        let opened = store(&tmp);
        let id = SessionId::from_string(uuidv7());
        let turn = TurnId::new();
        let model = ModelRef::new("local", "qwen");
        let mut session = opened.begin(header(&id)).unwrap();
        let mut emit_model_start = || {
          let mut started = EventEnvelope::new(
            meta(&id, &turn),
            AgentEvent::ModelRequestStarted(ModelRequestStarted {
              epoch: 0,
              model: model.clone(),
              message_count: 1,
              context_tokens_est: 12,
              tools_exposed: 3,
            }),
          );
          session.emit(&mut started).unwrap();
        };
        let (mut envelope, message) = match case {
          Case::User => (
            EventEnvelope::new(
              meta(&id, &turn),
              AgentEvent::UserMessage(UserMessage {
                text: "user message".into(),
                attachments: 0,
              }),
            ),
            Message::user("user message"),
          ),
          Case::ExternalContext => {
            let source = rupi_core::ExternalContextSource {
              provider: "fixture".into(),
              resource_id: "chunk-1".into(),
              provenance: "fixture/test".into(),
            };
            let mut metadata = BTreeMap::new();
            metadata.insert("url".into(), "https://example.test/chunk-1".into());
            let context = rupi_core::ExternalContextItem::inline(
              source,
              "evidence from the context provider",
              Some("[1]".into()),
            )
            .with_metadata(metadata.clone());
            (
              EventEnvelope::new(
                meta(&id, &turn),
                AgentEvent::ExternalContextRetrieved(rupi_core::ExternalContextRetrieved {
                  source: context.source.clone(),
                  citation: context.citation.clone(),
                  bytes: context.text.len() as u64,
                  inline: true,
                  metadata,
                }),
              ),
              Message::user(context.format_for_model()),
            )
          }
          Case::AssistantText => {
            emit_model_start();
            let mut delta = EventEnvelope::new(
              meta(&id, &turn),
              AgentEvent::AssistantDelta(rupi_core::AssistantDelta {
                text: "assistant text".into(),
                chunk_index: 0,
              }),
            );
            session.emit(&mut delta).unwrap();
            (
              EventEnvelope::new(
                meta(&id, &turn),
                AgentEvent::ModelRequestCompleted(ModelRequestCompleted {
                  epoch: 0,
                  model: model.clone(),
                  finish_reason: Some("stop".into()),
                  input_tokens: Some(12),
                  logical_prompt_tokens: Some(12),
                  cache_read_tokens: None,
                  cache_write_tokens: None,
                  output_tokens: Some(2),
                  provider_total_tokens: Some(14),
                  duration_ms: 1,
                  tool_calls: 0,
                  reasoning_provenance: None,
                  first_delta_ms: Some(0),
                }),
              ),
              Message::assistant("assistant text"),
            )
          }
          Case::AssistantCalls => {
            emit_model_start();
            let calls = vec![
              ToolCallBlock {
                id: ToolCallId::from_string("99999999-9999-4999-8999-999999999991"),
                name: "read".into(),
                arguments: serde_json::json!({"path": "a.txt"}),
              },
              ToolCallBlock {
                id: ToolCallId::from_string("99999999-9999-4999-8999-999999999992"),
                name: "exec".into(),
                arguments: serde_json::json!({"command": "echo ok"}),
              },
            ];
            (
              EventEnvelope::new(
                meta(&id, &turn),
                AgentEvent::ModelRequestCompleted(ModelRequestCompleted {
                  epoch: 0,
                  model: model.clone(),
                  finish_reason: Some("tool_calls".into()),
                  input_tokens: Some(12),
                  logical_prompt_tokens: Some(12),
                  cache_read_tokens: None,
                  cache_write_tokens: None,
                  output_tokens: Some(4),
                  provider_total_tokens: Some(16),
                  duration_ms: 1,
                  tool_calls: calls.len() as u32,
                  reasoning_provenance: None,
                  first_delta_ms: Some(0),
                }),
              ),
              Message::new(
                Role::Assistant,
                calls.into_iter().map(ContentBlock::ToolCall).collect(),
              ),
            )
          }
          Case::ToolCompleted | Case::ReducedToolCompleted => {
            let call_id = ToolCallId::from_string(if matches!(case, Case::ToolCompleted) {
              "99999999-9999-4999-8999-999999999993"
            } else {
              "99999999-9999-4999-8999-999999999994"
            });
            let name = "read";
            let mut requested = EventEnvelope::new(
              meta(&id, &turn),
              AgentEvent::ToolRequested(ToolRequested {
                call_id: call_id.clone(),
                name: name.into(),
                arguments: serde_json::json!({"path": "a.txt"}),
                read_only: true,
              }),
            );
            session.emit(&mut requested).unwrap();
            let mut started = EventEnvelope::new(
              meta(&id, &turn),
              AgentEvent::ToolStarted(ToolStarted {
                call_id: call_id.clone(),
                name: name.into(),
              }),
            );
            session.emit(&mut started).unwrap();
            let reduced = matches!(case, Case::ReducedToolCompleted);
            let text = if reduced { "visible" } else { "complete" };
            let blob = reduced.then(|| session.put_recovery_blob(b"full tool output").unwrap());
            (
              EventEnvelope::new(
                meta(&id, &turn),
                AgentEvent::ToolCompleted(ToolCompleted {
                  call_id: call_id.clone(),
                  name: name.into(),
                  state: ToolExecutionState::Succeeded,
                  duration_ms: 1,
                  status: Some(0),
                  reduced,
                  blob,
                  visible_bytes: text.len() as u64,
                }),
              ),
              Message::new(
                Role::Tool,
                vec![ContentBlock::ToolResult(ToolResultBlock {
                  id: call_id,
                  name: name.into(),
                  state: ToolExecutionState::Succeeded,
                  text: text.into(),
                  is_error: false,
                  reduced,
                })],
              ),
            )
          }
          Case::ToolFailed | Case::ToolUnknown => {
            let call_id = ToolCallId::from_string(if matches!(case, Case::ToolFailed) {
              "99999999-9999-4999-8999-999999999995"
            } else {
              "99999999-9999-4999-8999-999999999996"
            });
            let name = "write";
            let mut requested = EventEnvelope::new(
              meta(&id, &turn),
              AgentEvent::ToolRequested(ToolRequested {
                call_id: call_id.clone(),
                name: name.into(),
                arguments: serde_json::json!({"path": "out.txt", "contents": "x"}),
                read_only: false,
              }),
            );
            session.emit(&mut requested).unwrap();
            let (event, state, text) = if matches!(case, Case::ToolFailed) {
              (
                AgentEvent::ToolFailed(ToolFailed {
                  call_id: call_id.clone(),
                  name: name.into(),
                  message: "failed output".into(),
                  duration_ms: 1,
                  status: Some(1),
                }),
                ToolExecutionState::Failed,
                "failed output",
              )
            } else {
              (
                AgentEvent::ToolUnknown(rupi_core::ToolUnknown {
                  call_id: call_id.clone(),
                  name: name.into(),
                  why: "completion not observed".into(),
                  mutating: true,
                }),
                ToolExecutionState::Unknown,
                "completion not observed",
              )
            };
            (
              EventEnvelope::new(meta(&id, &turn), event),
              Message::new(
                Role::Tool,
                vec![ContentBlock::ToolResult(ToolResultBlock {
                  id: call_id,
                  name: name.into(),
                  state,
                  text: text.into(),
                  is_error: true,
                  reduced: false,
                })],
              ),
            )
          }
        };
        let event_id = envelope.meta.event_id.clone();
        session.set_failpoint(failpoint);
        assert!(
          session.emit_message(&mut envelope, &message).is_err(),
          "{case:?} must interrupt at {failpoint:?}"
        );
        drop(session);

        let resumed = opened
          .resume(&id)
          .unwrap_or_else(|error| panic!("{case:?} at {failpoint:?}: {error}"));
        let restored = opened.restore(&id).unwrap();
        if failpoint != ProjectionFailpoint::Prepare {
          let projection = restored
            .messages
            .iter()
            .find(|record| record.event_id == event_id)
            .unwrap_or_else(|| panic!("{case:?} projection is missing at {failpoint:?}"));
          assert_eq!(projection.message, message, "{case:?} at {failpoint:?}");
        }
        resumed.finish().unwrap();
      }
    }
  }

  #[test]
  fn oversized_message_recovery_uses_a_durable_blob_reference() {
    let tmp = TempDir::new("store-message-wal-bound");
    let opened = store(&tmp);
    let id = SessionId::from_string(uuidv7());
    let turn = TurnId::new();
    let mut session = opened.begin(header(&id)).unwrap();
    let text = "x".repeat(400_000);
    let mut envelope = EventEnvelope::new(
      meta(&id, &turn),
      AgentEvent::UserMessage(UserMessage {
        text: text.clone(),
        attachments: 0,
      }),
    );
    session.set_failpoint(ProjectionFailpoint::CanonicalAppend);
    assert!(
      session
        .emit_message(&mut envelope, &Message::user(&text))
        .is_err()
    );
    drop(session);

    let resumed = opened
      .resume(&id)
      .expect("message recovery must read its durable blob reference");
    let restored = opened.restore(&id).unwrap();
    assert_eq!(restored.messages.len(), 1);
    assert_eq!(restored.messages[0].message.text(), text);
    assert!(
      objects_under(&opened.layout().blobs_dir(&id))
        .iter()
        .any(|bytes| bytes.len() > 300_000),
      "large message bytes are persisted in the session blob store"
    );
    resumed.finish().unwrap();
  }

  #[test]
  fn oversized_semantic_message_is_rejected_before_durable_mutation() {
    let tmp = TempDir::new("store-message-line-bound");
    let opened = store(&tmp);
    let id = SessionId::from_string(uuidv7());
    let turn = TurnId::new();
    let mut session = opened.begin(header(&id)).unwrap();
    let text = "x".repeat(crate::jsonl::MAX_JSONL_LINE_BYTES + 1);
    let mut envelope = EventEnvelope::new(
      meta(&id, &turn),
      AgentEvent::UserMessage(UserMessage {
        text: text.clone(),
        attachments: 0,
      }),
    );
    let trace_path = session.trace_path().to_path_buf();
    let error = session
      .emit_message(&mut envelope, &Message::user(&text))
      .expect_err("semantic lines over the reader bound must be refused");
    assert!(error.to_string().contains("JSONL line bound"), "{error}");
    drop(session);

    assert!(TraceJournal::read(&trace_path).unwrap().items.is_empty());
    assert!(
      ProjectionWal::pending_at(&opened.layout().wal_path(&id))
        .unwrap()
        .is_empty()
    );
    assert!(object_paths_under(&opened.layout().blobs_dir(&id)).is_empty());
  }

  #[test]
  fn corrupt_message_recovery_blob_fails_closed() {
    let tmp = TempDir::new("store-message-wal-corrupt");
    let opened = store(&tmp);
    let id = SessionId::from_string(uuidv7());
    let turn = TurnId::new();
    let mut session = opened.begin(header(&id)).unwrap();
    let text = "x".repeat(400_000);
    let mut envelope = EventEnvelope::new(
      meta(&id, &turn),
      AgentEvent::UserMessage(UserMessage {
        text: text.clone(),
        attachments: 0,
      }),
    );
    session.set_failpoint(ProjectionFailpoint::CanonicalAppend);
    assert!(
      session
        .emit_message(&mut envelope, &Message::user(&text))
        .is_err()
    );
    drop(session);
    let blob = object_paths_under(&opened.layout().blobs_dir(&id))
      .into_iter()
      .find(|path| std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > 300_000))
      .expect("message recovery stores a large payload as a blob");
    std::fs::write(blob, b"corrupt").unwrap();
    let error = opened
      .resume(&id)
      .expect_err("a recovery blob hash mismatch must not become a message");
    assert!(
      error.to_string().contains("message recovery blob")
        || error.to_string().contains("blob reference does not match"),
      "{error}"
    );
  }

  #[test]
  fn oversized_held_message_recovers_without_a_projection_wal_payload() {
    let tmp = TempDir::new("store-message-wal-bound");
    let opened = store(&tmp);
    let id = SessionId::from_string(uuidv7());
    let turn = TurnId::new();
    let mut session = opened.begin(header(&id)).unwrap();
    let text = "x".repeat(400_000);
    let mut envelope = EventEnvelope::new(
      meta(&id, &turn),
      AgentEvent::UserMessage(UserMessage {
        text: text.clone(),
        attachments: 0,
      }),
    );
    session
      .emit_transaction(&mut envelope, None, true)
      .expect("held message intent is compact");
    drop(session);

    opened
      .resume(&id)
      .expect("message recovery must commit without copying its payload into the WAL")
      .finish()
      .unwrap();
    let restored = opened.restore(&id).unwrap();
    assert_eq!(restored.messages.len(), 1);
    assert_eq!(restored.messages[0].message.text(), text);
  }

  #[test]
  fn resuming_a_legacy_reduction_migrates_before_the_next_append() {
    let tmp = TempDir::new("store-legacy-reduction");
    let opened = store(&tmp);
    let id = SessionId::from_string(uuidv7());
    let turn = TurnId::new();
    {
      let mut session = opened.begin(header(&id)).unwrap();
      let mut user = EventEnvelope::new(
        meta(&id, &turn),
        AgentEvent::UserMessage(UserMessage {
          text: "legacy context".into(),
          attachments: 0,
        }),
      );
      session.emit(&mut user).unwrap();
      session
        .append_message(
          &turn,
          &rupi_core::message::Message::user("legacy context"),
          0,
          &ModelRef::new("local", "qwen"),
          &user,
        )
        .unwrap();
      let blob = session.put_recovery_blob(b"legacy full context").unwrap();
      let mut reduced = EventEnvelope::new(
        meta(&id, &turn),
        AgentEvent::ContextReduced(ContextReduced {
          reason: ReductionReason::RecentTargetExceeded { target_tokens: 128 },
          original_bytes: 512,
          visible_bytes: 128,
          removed_messages: 1,
          retained_messages: 0,
          recovery_ref: Some(blob.recovery_ref()),
          blob: Some(blob),
          tool_call_id: None,
        }),
      );
      let seq = session.emit(&mut reduced).unwrap();
      session
        .record(&SessionRecord::Reduction(SessionReductionRecord {
          event_id: reduced.meta.event_id.clone(),
          seq: Some(seq),
          reason: ReductionReason::RecentTargetExceeded { target_tokens: 128 },
          removed_messages: 1,
          retained_messages: 0,
        }))
        .unwrap();
      session.finish().unwrap();
    }

    let path = opened.layout().session_path(&id);
    let mut records: Vec<SessionRecord> = std::fs::read_to_string(&path)
      .unwrap()
      .lines()
      .map(|line| serde_json::from_str(line).unwrap())
      .collect();
    let SessionRecord::Header(header) = &mut records[0] else {
      panic!("test session starts with a header");
    };
    header.version = 1;
    let rewritten = records
      .iter()
      .map(|record| serde_json::to_string(record).unwrap())
      .collect::<Vec<_>>()
      .join("\n");
    std::fs::write(&path, format!("{rewritten}\n")).unwrap();

    let resumed = opened.resume(&id).expect("legacy projection is migrated");
    assert_eq!(resumed.header().version, SESSION_SCHEMA_VERSION);
    resumed.finish().unwrap();
    let restored = opened.restore(&id).unwrap();
    assert_eq!(restored.reductions.len(), 1);
    assert_eq!(restored.reductions[0].removed_messages, 1);
  }

  #[test]
  fn abandoned_model_request_is_closed_on_resume_without_retrying() {
    let tmp = TempDir::new("store-abandoned-model");
    let opened = store(&tmp);
    let id = SessionId::from_string(uuidv7());
    let turn = TurnId::new();
    {
      let mut session = opened.begin(header(&id)).unwrap();
      let mut started = EventEnvelope::new(
        meta(&id, &turn),
        AgentEvent::ModelRequestStarted(ModelRequestStarted {
          epoch: 0,
          model: ModelRef::new("local", "qwen"),
          message_count: 1,
          context_tokens_est: 12,
          tools_exposed: 0,
        }),
      );
      session.emit(&mut started).unwrap();
    }

    let resumed = opened
      .resume(&id)
      .expect("an interrupted provider request is safely abandoned");
    let trace = TraceJournal::read(resumed.trace_path()).unwrap();
    assert!(trace.items.iter().any(|entry| {
      matches!(
        &entry.envelope.event,
        AgentEvent::ModelRequestCompleted(completed)
          if completed.finish_reason.as_deref() == Some("abandoned")
      )
    }));
    assert!(trace.items.iter().any(|entry| {
      matches!(
        &entry.envelope.event,
        AgentEvent::Diagnostic(diagnostic) if diagnostic.message.contains("interrupted")
      )
    }));
    let restored = opened.restore(&id).unwrap();
    assert!(restored.messages.is_empty());
    resumed.finish().unwrap();
  }

  #[test]
  fn unstarted_tool_request_is_closed_as_a_failed_result_on_resume() {
    let tmp = TempDir::new("store-unstarted-tool");
    let opened = store(&tmp);
    let id = SessionId::from_string(uuidv7());
    let turn = TurnId::new();
    let call_id = ToolCallId::from_string("77777777-7777-4777-8777-777777777777");
    {
      let mut session = opened.begin(header(&id)).unwrap();
      let mut requested = EventEnvelope::new(
        meta(&id, &turn),
        AgentEvent::ToolRequested(ToolRequested {
          call_id: call_id.clone(),
          name: "write".into(),
          arguments: serde_json::json!({"path": "out.txt", "contents": "data"}),
          read_only: false,
        }),
      );
      session.emit(&mut requested).unwrap();
    }

    let resumed = opened
      .resume(&id)
      .expect("absence of ToolStarted proves no tool code ran");
    let restored = opened.restore(&id).unwrap();
    assert_eq!(restored.interrupted_tools.len(), 0);
    assert_eq!(restored.messages.len(), 1);
    assert_eq!(restored.messages[0].role, Role::Tool);
    assert!(matches!(
      restored.messages[0].message.content.first(),
      Some(ContentBlock::ToolResult(result)) if result.text.contains("no side effect")
    ));
    let trace = TraceJournal::read(resumed.trace_path()).unwrap();
    assert!(trace.items.iter().any(|entry| {
      matches!(
        &entry.envelope.event,
        AgentEvent::ToolFailed(failed) if failed.call_id == call_id
      )
    }));
    resumed.finish().unwrap();
  }

  #[test]
  fn assistant_tool_call_without_a_request_is_closed_as_never_executed() {
    let tmp = TempDir::new("store-assistant-call-recovery");
    let opened = store(&tmp);
    let id = SessionId::from_string(uuidv7());
    let turn = TurnId::new();
    let call_id = ToolCallId::from_string("88888888-8888-4888-8888-888888888888");
    {
      let mut session = opened.begin(header(&id)).unwrap();
      let mut started = EventEnvelope::new(
        meta(&id, &turn),
        AgentEvent::ModelRequestStarted(ModelRequestStarted {
          epoch: 0,
          model: ModelRef::new("local", "qwen"),
          message_count: 1,
          context_tokens_est: 12,
          tools_exposed: 1,
        }),
      );
      session.emit(&mut started).unwrap();
      let call = ToolCallBlock {
        id: call_id.clone(),
        name: "write".into(),
        arguments: serde_json::json!({"path": "out.txt", "contents": "data"}),
      };
      let assistant = Message::new(Role::Assistant, vec![ContentBlock::ToolCall(call)]);
      let mut completion = EventEnvelope::new(
        meta(&id, &turn),
        AgentEvent::ModelRequestCompleted(ModelRequestCompleted {
          epoch: 0,
          model: ModelRef::new("local", "qwen"),
          finish_reason: Some("tool_calls".into()),
          input_tokens: Some(12),
          logical_prompt_tokens: Some(12),
          cache_read_tokens: None,
          cache_write_tokens: None,
          output_tokens: Some(4),
          provider_total_tokens: Some(16),
          duration_ms: 1,
          tool_calls: 1,
          reasoning_provenance: None,
          first_delta_ms: Some(0),
        }),
      );
      session.emit_message(&mut completion, &assistant).unwrap();
    }

    let resumed = opened
      .resume(&id)
      .expect("an assistant call without a request is provably unexecuted");
    let trace = TraceJournal::read(resumed.trace_path()).unwrap();
    let assistant_event = trace
      .items
      .iter()
      .find(|entry| matches!(entry.envelope.event, AgentEvent::ModelRequestCompleted(_)))
      .unwrap();
    let requested = trace
      .items
      .iter()
      .find(|entry| {
        matches!(
          &entry.envelope.event,
          AgentEvent::ToolRequested(requested) if requested.call_id == call_id
        )
      })
      .expect("resume synthesizes the missing request");
    assert_eq!(
      requested.envelope.meta.parent_event_id.as_ref(),
      Some(&assistant_event.envelope.meta.event_id),
      "the recovery-generated request is parented to the assistant message event"
    );
    assert!(trace.items.iter().any(|entry| {
      matches!(
        &entry.envelope.event,
        AgentEvent::ToolFailed(failed)
          if failed.call_id == call_id
            && failed.message == "not executed: process stopped before execution boundary"
      )
    }));
    let restored = opened.restore(&id).unwrap();
    assert_eq!(restored.messages.len(), 2);
    assert!(matches!(
      restored.messages[1].message.content.first(),
      Some(ContentBlock::ToolResult(result))
        if result.id == call_id
          && result.text == "not executed: process stopped before execution boundary"
    ));
    resumed.finish().unwrap();
  }

  #[test]
  fn assistant_tool_matching_restores_externalized_request_arguments() {
    let tmp = TempDir::new("store-assistant-externalized-request");
    let opened = store(&tmp);
    let id = SessionId::from_string(uuidv7());
    let turn = TurnId::new();
    let call_id = ToolCallId::from_string("88888888-8888-4888-8888-888888888886");
    let arguments = serde_json::json!({"payload": "x".repeat(20_000)});
    {
      let mut session = opened.begin(header(&id)).unwrap();
      let mut started = EventEnvelope::new(
        meta(&id, &turn),
        AgentEvent::ModelRequestStarted(ModelRequestStarted {
          epoch: 0,
          model: ModelRef::new("local", "qwen"),
          message_count: 1,
          context_tokens_est: 12,
          tools_exposed: 1,
        }),
      );
      session.emit(&mut started).unwrap();
      let assistant = Message::new(
        Role::Assistant,
        vec![ContentBlock::ToolCall(ToolCallBlock {
          id: call_id.clone(),
          name: "read".into(),
          arguments: arguments.clone(),
        })],
      );
      let mut completion = EventEnvelope::new(
        meta(&id, &turn),
        AgentEvent::ModelRequestCompleted(ModelRequestCompleted {
          epoch: 0,
          model: ModelRef::new("local", "qwen"),
          finish_reason: Some("tool_calls".into()),
          input_tokens: Some(12),
          logical_prompt_tokens: Some(12),
          cache_read_tokens: None,
          cache_write_tokens: None,
          output_tokens: Some(4),
          provider_total_tokens: Some(16),
          duration_ms: 1,
          tool_calls: 1,
          reasoning_provenance: None,
          first_delta_ms: Some(0),
        }),
      );
      session.emit_message(&mut completion, &assistant).unwrap();
      let mut requested = EventEnvelope::new(
        meta(&id, &turn),
        AgentEvent::ToolRequested(ToolRequested {
          call_id,
          name: "read".into(),
          arguments,
          read_only: true,
        }),
      );
      session.emit(&mut requested).unwrap();
    }

    let resumed = opened
      .resume(&id)
      .expect("externalized request arguments must compare after blob restoration");
    let trace = TraceJournal::read(resumed.trace_path()).unwrap();
    assert_eq!(
      trace
        .items
        .iter()
        .filter(|entry| matches!(entry.envelope.event, AgentEvent::ToolRequested(_)))
        .count(),
      1,
      "matching externalized request is not synthesized a second time"
    );
    assert!(
      trace
        .items
        .iter()
        .any(|entry| { matches!(entry.envelope.event, AgentEvent::ToolFailed(_)) })
    );
    resumed.finish().unwrap();
  }

  #[test]
  fn assistant_multi_tool_recovery_preserves_completed_calls_and_closes_only_missing_calls() {
    let tmp = TempDir::new("store-assistant-multi-recovery");
    let opened = store(&tmp);
    let id = SessionId::from_string(uuidv7());
    let turn = TurnId::new();
    let calls = [
      ToolCallBlock {
        id: ToolCallId::from_string("88888888-8888-4888-8888-888888888881"),
        name: "read".into(),
        arguments: serde_json::json!({"path": "a.txt"}),
      },
      ToolCallBlock {
        id: ToolCallId::from_string("88888888-8888-4888-8888-888888888882"),
        name: "write".into(),
        arguments: serde_json::json!({"path": "b.txt", "contents": "b"}),
      },
      ToolCallBlock {
        id: ToolCallId::from_string("88888888-8888-4888-8888-888888888883"),
        name: "exec".into(),
        arguments: serde_json::json!({"command": "echo c"}),
      },
    ];
    {
      let mut session = opened.begin(header(&id)).unwrap();
      let mut started = EventEnvelope::new(
        meta(&id, &turn),
        AgentEvent::ModelRequestStarted(ModelRequestStarted {
          epoch: 0,
          model: ModelRef::new("local", "qwen"),
          message_count: 1,
          context_tokens_est: 12,
          tools_exposed: 3,
        }),
      );
      session.emit(&mut started).unwrap();
      let assistant = Message::new(
        Role::Assistant,
        calls.iter().cloned().map(ContentBlock::ToolCall).collect(),
      );
      let mut completion = EventEnvelope::new(
        meta(&id, &turn),
        AgentEvent::ModelRequestCompleted(ModelRequestCompleted {
          epoch: 0,
          model: ModelRef::new("local", "qwen"),
          finish_reason: Some("tool_calls".into()),
          input_tokens: Some(12),
          logical_prompt_tokens: Some(12),
          cache_read_tokens: None,
          cache_write_tokens: None,
          output_tokens: Some(6),
          provider_total_tokens: Some(18),
          duration_ms: 1,
          tool_calls: 3,
          reasoning_provenance: None,
          first_delta_ms: Some(0),
        }),
      );
      session.emit_message(&mut completion, &assistant).unwrap();

      let mut requested = EventEnvelope::new(
        meta(&id, &turn),
        AgentEvent::ToolRequested(ToolRequested {
          call_id: calls[0].id.clone(),
          name: calls[0].name.clone(),
          arguments: calls[0].arguments.clone(),
          read_only: true,
        }),
      );
      session.emit(&mut requested).unwrap();
      let mut tool_started = EventEnvelope::new(
        meta(&id, &turn),
        AgentEvent::ToolStarted(ToolStarted {
          call_id: calls[0].id.clone(),
          name: calls[0].name.clone(),
        }),
      );
      session.emit(&mut tool_started).unwrap();
      let mut completed = EventEnvelope::new(
        meta(&id, &turn),
        AgentEvent::ToolCompleted(ToolCompleted {
          call_id: calls[0].id.clone(),
          name: calls[0].name.clone(),
          state: ToolExecutionState::Succeeded,
          duration_ms: 1,
          status: Some(0),
          reduced: false,
          blob: None,
          visible_bytes: 2,
        }),
      );
      session
        .emit_message(
          &mut completed,
          &Message::new(
            Role::Tool,
            vec![ContentBlock::ToolResult(ToolResultBlock {
              id: calls[0].id.clone(),
              name: calls[0].name.clone(),
              state: ToolExecutionState::Succeeded,
              text: "ok".into(),
              is_error: false,
              reduced: false,
            })],
          ),
        )
        .unwrap();
    }

    let resumed = opened.resume(&id).unwrap();
    let trace = TraceJournal::read(resumed.trace_path()).unwrap();
    assert_eq!(
      trace
        .items
        .iter()
        .filter(|entry| matches!(entry.envelope.event, AgentEvent::ToolRequested(_)))
        .count(),
      3,
      "the already completed call is not duplicated"
    );
    for call in &calls[1..] {
      assert!(trace.items.iter().any(|entry| {
        matches!(
          &entry.envelope.event,
          AgentEvent::ToolFailed(failed)
            if failed.call_id == call.id
              && failed.message == "not executed: process stopped before execution boundary"
        )
      }));
    }
    let restored = opened.restore(&id).unwrap();
    assert_eq!(restored.messages.len(), 4);
    assert!(matches!(
      restored.messages[1].message.content.first(),
      Some(ContentBlock::ToolResult(result))
        if result.id == calls[0].id && result.state == ToolExecutionState::Succeeded
    ));
    for (index, call) in calls[1..].iter().enumerate() {
      assert!(matches!(
        restored.messages[index + 2].message.content.first(),
        Some(ContentBlock::ToolResult(result))
          if result.id == call.id
            && result.state == ToolExecutionState::Failed
      ));
    }
    resumed.finish().unwrap();
  }

  #[test]
  fn assistant_tool_request_duplicates_and_mismatches_fail_closed() {
    for (label, second_arguments) in [
      ("duplicate", serde_json::json!({"path": "out.txt"})),
      ("mismatch", serde_json::json!({"path": "other.txt"})),
    ] {
      let tmp = TempDir::new(&format!("store-assistant-{label}"));
      let opened = store(&tmp);
      let id = SessionId::from_string(uuidv7());
      let turn = TurnId::new();
      let call_id = ToolCallId::from_string("88888888-8888-4888-8888-888888888887");
      let mut session = opened.begin(header(&id)).unwrap();
      let mut started = EventEnvelope::new(
        meta(&id, &turn),
        AgentEvent::ModelRequestStarted(ModelRequestStarted {
          epoch: 0,
          model: ModelRef::new("local", "qwen"),
          message_count: 1,
          context_tokens_est: 12,
          tools_exposed: 1,
        }),
      );
      session.emit(&mut started).unwrap();
      let call = ToolCallBlock {
        id: call_id.clone(),
        name: "write".into(),
        arguments: serde_json::json!({"path": "out.txt"}),
      };
      let assistant = Message::new(Role::Assistant, vec![ContentBlock::ToolCall(call)]);
      let mut completion = EventEnvelope::new(
        meta(&id, &turn),
        AgentEvent::ModelRequestCompleted(ModelRequestCompleted {
          epoch: 0,
          model: ModelRef::new("local", "qwen"),
          finish_reason: Some("tool_calls".into()),
          input_tokens: Some(12),
          logical_prompt_tokens: Some(12),
          cache_read_tokens: None,
          cache_write_tokens: None,
          output_tokens: Some(4),
          provider_total_tokens: Some(16),
          duration_ms: 1,
          tool_calls: 1,
          reasoning_provenance: None,
          first_delta_ms: Some(0),
        }),
      );
      session.emit_message(&mut completion, &assistant).unwrap();
      for arguments in [serde_json::json!({"path": "out.txt"}), second_arguments] {
        let mut requested = EventEnvelope::new(
          meta(&id, &turn),
          AgentEvent::ToolRequested(ToolRequested {
            call_id: call_id.clone(),
            name: "write".into(),
            arguments,
            read_only: false,
          }),
        );
        session.emit(&mut requested).unwrap();
      }
      drop(session);
      let error = opened
        .resume(&id)
        .expect_err("ambiguous assistant call reconciliation must fail closed");
      assert!(
        error.to_string().contains("duplicate canonical requests")
          || error
            .to_string()
            .contains("disagrees with its canonical request"),
        "{label}: {error}"
      );
    }
  }

  #[test]
  fn oversized_checkpoint_projection_is_rejected_before_canonical_append() {
    let tmp = TempDir::new("store-checkpoint-wal-bound");
    let opened = store(&tmp);
    let id = SessionId::from_string(uuidv7());
    let mut session = opened.begin(header(&id)).unwrap();
    let capsule = capsule(&"x".repeat(400_000));
    let error = session
      .prepare_checkpoint(&capsule)
      .expect_err("an oversized capsule cannot enter durable state");
    assert!(format!("{error}").contains("checkpoint capsule exceeds"));
    assert!(
      TraceJournal::read(session.trace_path())
        .unwrap()
        .items
        .is_empty(),
      "capsule preflight must reject before the canonical event is appended"
    );
    session.finish().unwrap();
    assert!(
      opened.list_checkpoints(&id).unwrap().is_empty(),
      "the prepared orphan is not a committed checkpoint"
    );
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
      vec![b"{\"api_key\":\"[redacted:field]\"}".to_vec()],
      "captured bytes are redacted before storage under the session blob directory"
    );
  }

  #[test]
  fn structured_credentials_are_redacted_in_session_payloads() {
    let tmp = TempDir::new("store-structured-redaction");
    let opened = Store::open(
      tmp.path(),
      WritePolicy {
        redaction: RedactionPolicy {
          scan_environment: false,
          ..RedactionPolicy::default()
        },
        ..WritePolicy::default()
      },
    )
    .unwrap();
    let id = SessionId::from_string(uuidv7());
    let mut session = opened.begin(header(&id)).unwrap();

    let inline = session
      .put_payload(br#"{"api_key":"plain-secret"}"#)
      .unwrap();
    assert_eq!(
      inline,
      Payload::Inline(r#"{"api_key":"[redacted:field]"}"#.into())
    );

    let large_json = format!(
      r#"{{"token":"plain-secret","padding":"{}"}}"#,
      "x".repeat(9_000)
    );
    let stored = session.put_payload(large_json.as_bytes()).unwrap();
    let blob = stored.blob().expect("large payload is filed as a blob");
    let value: serde_json::Value = serde_json::from_slice(&session.blobs().get(blob).unwrap())
      .expect("redacted blob remains valid JSON");
    assert_eq!(value["token"], "[redacted:field]");
    assert!(!value.to_string().contains("plain-secret"));

    let recovery = session
      .put_recovery_blob(br#"{"password":"plain-secret"}"#)
      .unwrap();
    let recovered: serde_json::Value =
      serde_json::from_slice(&session.blobs().get(&recovery).unwrap()).unwrap();
    assert_eq!(recovered["password"], "[redacted:field]");
    assert!(!recovered.to_string().contains("plain-secret"));

    let binary = [0xff, b'x', 0x80];
    let binary_blob = session.put_recovery_blob(&binary).unwrap();
    assert_eq!(session.blobs().get(&binary_blob).unwrap(), binary);
  }

  #[test]
  fn checkpoint_archive_is_redacted_before_standalone_write() {
    let tmp = TempDir::new("store-checkpoint-redaction");
    let secret = "checkpoint-secret-123";
    let opened = Store::open(
      tmp.path(),
      WritePolicy {
        redaction: RedactionPolicy {
          literals: vec![secret.into()],
          scan_environment: false,
          ..RedactionPolicy::default()
        },
        ..WritePolicy::default()
      },
    )
    .unwrap();
    let id = SessionId::from_string(uuidv7());
    let mut session = opened.begin(header(&id)).unwrap();
    let barrier = session.checkpoint(&capsule(secret)).unwrap();
    let path = opened.layout().checkpoint_path(&id, &barrier.checkpoint_id);
    let bytes = std::fs::read(&path).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains(secret));
    let standalone: ContextCapsule = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(standalone.objective, "[redacted:field]");

    let restored = opened.restore(&id).unwrap();
    assert_eq!(
      restored.checkpoint.unwrap().objective,
      "[redacted:field]",
      "the duplicated barrier uses the same durable redaction boundary"
    );
  }

  /// Every stored object below a directory, descending into hash shards.
  fn objects_under(dir: &std::path::Path) -> Vec<Vec<u8>> {
    let mut objects = object_paths_under(dir)
      .into_iter()
      .map(|path| std::fs::read(path).unwrap())
      .collect::<Vec<_>>();
    objects.sort();
    objects
  }

  fn object_paths_under(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut objects = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap().filter_map(Result::ok) {
      if entry.path().is_file() {
        objects.push(entry.path());
      } else {
        objects.extend(object_paths_under(&entry.path()));
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
  fn configured_compression_redacts_before_encoding_and_recovers_logical_bytes() {
    let tmp = TempDir::new("store-compression-redaction");
    let policy = WritePolicy {
      compression: BlobCompression::Deflate,
      redaction: RedactionPolicy {
        literals: vec!["secret-value".into()],
        scan_environment: false,
        ..RedactionPolicy::default()
      },
      ..WritePolicy::default()
    };
    let opened = Store::open(tmp.path(), policy).unwrap();
    let id = SessionId::from_string(uuidv7());
    let session = opened.begin(header(&id)).unwrap();
    let blob = session
      .put_recovery_blob(format!("secret-value {}", "repeat ".repeat(2_000)).as_bytes())
      .unwrap();
    let encoded = std::fs::read(session.blobs().path_for(&blob)).unwrap();

    assert_eq!(blob.compression, BlobCompression::Deflate);
    assert!(
      !encoded
        .windows("secret-value".len())
        .any(|window| window == b"secret-value")
    );
    let recovered = session.blobs().get(&blob).unwrap();
    assert!(
      String::from_utf8(recovered)
        .unwrap()
        .starts_with("[redacted:field]")
    );

    let public = opened.blobs(&id).unwrap();
    let public_blob = public
      .put(&b"public repeated payload ".repeat(256), None)
      .unwrap();
    assert_eq!(public_blob.compression, BlobCompression::Deflate);
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
        text: "implement the durable store for rupi, including journals and blobs".into(),
        attachments: 0,
      }),
    );
    session.emit(&mut introduced).unwrap();
    session
      .append_message(
        &turn,
        &rupi_core::message::Message::user(
          "implement the durable store for rupi, including journals and blobs",
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
    assert_eq!(derived.compression, BlobCompression::None);
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
  fn wal_repairs_a_trace_event_when_the_projection_append_was_interrupted() {
    let tmp = TempDir::new("store-wal-recovery");
    let opened = store(&tmp);
    let session_id = SessionId::new();
    let turn_id = TurnId::new();
    let mut session = opened.begin(header(&session_id)).unwrap();
    let mut envelope = EventEnvelope::new(
      meta(&session_id, &turn_id),
      AgentEvent::UserMessage(UserMessage {
        text: "durable before crash".into(),
        attachments: 0,
      }),
    );
    session
      .emit_transaction(&mut envelope, None, true)
      .expect("trace and WAL prepare are durable");
    drop(session);

    assert!(
      matches!(opened.restore(&session_id), Err(StoreError::Invalid(_))),
      "read-only restore must not continue from a projection with an open intent"
    );
    opened
      .resume(&session_id)
      .expect("resume repairs the missing semantic record")
      .finish()
      .unwrap();
    let restored = opened.restore(&session_id).unwrap();
    assert_eq!(restored.messages.len(), 1);
    assert_eq!(restored.messages[0].message.text(), "durable before crash");
  }

  #[test]
  fn wal_recovery_restores_an_externalized_user_message_exactly() {
    let tmp = TempDir::new("store-wal-externalized-user");
    let opened = store(&tmp);
    let session_id = SessionId::new();
    let turn_id = TurnId::new();
    let original = "user history ".repeat(20_000);
    let mut session = opened.begin(header(&session_id)).unwrap();
    let mut envelope = EventEnvelope::new(
      meta(&session_id, &turn_id),
      AgentEvent::UserMessage(UserMessage {
        text: original.clone(),
        attachments: 0,
      }),
    );
    session
      .emit_transaction(&mut envelope, None, true)
      .expect("trace and WAL prepare are durable");
    drop(session);

    opened
      .resume(&session_id)
      .expect("resume reconstructs the blob-backed user projection")
      .finish()
      .unwrap();
    let restored = opened.restore(&session_id).unwrap();
    assert_eq!(restored.messages.len(), 1);
    assert_eq!(restored.messages[0].message.text(), original);
  }

  #[test]
  fn wal_recovery_reconstructs_only_the_assistant_deltas_of_its_request() {
    let tmp = TempDir::new("store-wal-assistant");
    let opened = store(&tmp);
    let session_id = SessionId::new();
    let turn_id = TurnId::new();
    let mut session = opened.begin(header(&session_id)).unwrap();

    let mut request_start = EventEnvelope::new(
      meta(&session_id, &turn_id),
      AgentEvent::ModelRequestStarted(rupi_core::ModelRequestStarted {
        epoch: 0,
        model: ModelRef::new("local", "qwen"),
        message_count: 1,
        context_tokens_est: 1,
        tools_exposed: 0,
      }),
    );
    session.emit(&mut request_start).unwrap();
    let mut first_delta = EventEnvelope::new(
      meta(&session_id, &turn_id),
      AgentEvent::AssistantDelta(rupi_core::AssistantDelta {
        text: "only this answer".into(),
        chunk_index: 0,
      }),
    );
    session.emit(&mut first_delta).unwrap();
    session
      .emit(&mut EventEnvelope::new(
        meta(&session_id, &turn_id),
        AgentEvent::ModelRequestCompleted(rupi_core::ModelRequestCompleted {
          epoch: 0,
          model: ModelRef::new("local", "qwen"),
          finish_reason: None,
          input_tokens: None,
          logical_prompt_tokens: None,
          cache_read_tokens: None,
          cache_write_tokens: None,
          output_tokens: None,
          provider_total_tokens: None,
          duration_ms: 1,
          tool_calls: 0,
          reasoning_provenance: None,
          first_delta_ms: Some(1),
        }),
      ))
      .unwrap();

    // A prior request in the same turn/epoch must not be merged into recovery.
    let mut prior_start = EventEnvelope::new(
      meta(&session_id, &turn_id),
      AgentEvent::ModelRequestStarted(rupi_core::ModelRequestStarted {
        epoch: 0,
        model: ModelRef::new("local", "qwen"),
        message_count: 1,
        context_tokens_est: 1,
        tools_exposed: 0,
      }),
    );
    session.emit(&mut prior_start).unwrap();
    let mut prior_delta = EventEnvelope::new(
      meta(&session_id, &turn_id),
      AgentEvent::AssistantDelta(rupi_core::AssistantDelta {
        text: "must not merge".into(),
        chunk_index: 0,
      }),
    );
    session.emit(&mut prior_delta).unwrap();
    let mut completion = EventEnvelope::new(
      meta(&session_id, &turn_id),
      AgentEvent::ModelRequestCompleted(rupi_core::ModelRequestCompleted {
        epoch: 0,
        model: ModelRef::new("local", "qwen"),
        finish_reason: Some("stop".into()),
        input_tokens: Some(1),
        logical_prompt_tokens: Some(1),
        cache_read_tokens: None,
        cache_write_tokens: None,
        output_tokens: Some(3),
        provider_total_tokens: Some(4),
        duration_ms: 1,
        tool_calls: 0,
        reasoning_provenance: None,
        first_delta_ms: Some(1),
      }),
    );
    session
      .emit_transaction(&mut completion, None, true)
      .expect("completion leaves a recoverable WAL intent");
    drop(session);

    opened
      .resume(&session_id)
      .expect("resume reconstructs the missing assistant projection")
      .finish()
      .unwrap();
    let restored = opened.restore(&session_id).unwrap();
    assert_eq!(restored.messages.len(), 1);
    assert_eq!(restored.messages[0].message.text(), "must not merge");
  }

  #[test]
  fn wal_recovery_preserves_summary_compaction_bounds() {
    let tmp = TempDir::new("store-wal-compaction");
    let opened = store(&tmp);
    let session_id = SessionId::new();
    let turn_id = TurnId::new();
    let model = ModelRef::new("local", "qwen");
    let mut session = opened.begin(header(&session_id)).unwrap();
    let mut messages = Vec::new();
    for text in ["old one", "old two"] {
      let mut envelope = EventEnvelope::new(
        meta(&session_id, &turn_id),
        AgentEvent::UserMessage(UserMessage {
          text: text.into(),
          attachments: 0,
        }),
      );
      session.emit(&mut envelope).unwrap();
      session
        .append_message(&turn_id, &Message::user(text), 0, &model, &envelope)
        .unwrap();
      messages.push(envelope);
    }
    session
      .emit(&mut EventEnvelope::new(
        meta(&session_id, &turn_id),
        AgentEvent::ContextCompactionStarted(rupi_core::ContextCompactionStarted {
          level: rupi_core::ContextLevel::L1Ordinary,
          reason: "test".into(),
        }),
      ))
      .unwrap();
    let summary = Message::user("summary");
    let mut summary_envelope =
      EventEnvelope::new(meta(&session_id, &turn_id), AgentEvent::ContextSummary);
    session.emit(&mut summary_envelope).unwrap();
    session
      .append_message(&turn_id, &summary, 0, &model, &summary_envelope)
      .unwrap();
    let mut epoch = EventEnvelope::new(
      meta(&session_id, &turn_id),
      AgentEvent::ContextCompactionEpoch(rupi_core::ContextCompactionEpoch {
        context_epoch: 1,
        replaces_from: messages[0].meta.seq.unwrap(),
        replaces_through: messages[1].meta.seq.unwrap(),
        summary: None,
      }),
    );
    session.emit(&mut epoch).unwrap();
    let mut completion = EventEnvelope::new(
      meta(&session_id, &turn_id),
      AgentEvent::ContextCompactionCompleted(rupi_core::ContextCompactionCompleted {
        level: rupi_core::ContextLevel::L1Ordinary,
        removed_messages: 2,
        retained_messages: 0,
        context_epoch: 1,
      }),
    );
    session
      .emit_transaction(&mut completion, None, true)
      .unwrap();
    drop(session);

    opened
      .resume(&session_id)
      .expect("canonical epoch recovers the missing projection bounds")
      .finish()
      .unwrap();
    let restored = opened.restore(&session_id).unwrap();
    assert_eq!(restored.messages.len(), 1);
    assert_eq!(restored.messages[0].message.text(), "summary");
  }

  #[test]
  fn restore_rejects_a_trace_message_without_its_projection() {
    let tmp = TempDir::new("store-projection-missing");
    let opened = store(&tmp);
    let session_id = SessionId::new();
    let turn_id = TurnId::new();
    let mut session = opened.begin(header(&session_id)).unwrap();
    session
      .emit(&mut EventEnvelope::new(
        meta(&session_id, &turn_id),
        AgentEvent::SessionStarted(SessionStarted {
          working_dir: "/repo".into(),
          model: ModelRef::new("local", "qwen"),
          capabilities: rupi_core::ModelCapabilities::text_only(8_192),
          resumed: false,
        }),
      ))
      .unwrap();
    let mut first = EventEnvelope::new(
      meta(&session_id, &turn_id),
      AgentEvent::UserMessage(UserMessage {
        text: "first".into(),
        attachments: 0,
      }),
    );
    session.emit(&mut first).unwrap();
    session
      .append_message(
        &turn_id,
        &Message::user("first"),
        0,
        &ModelRef::new("local", "qwen"),
        &first,
      )
      .unwrap();
    let mut second = EventEnvelope::new(
      meta(&session_id, &turn_id),
      AgentEvent::UserMessage(UserMessage {
        text: "second".into(),
        attachments: 0,
      }),
    );
    session.emit(&mut second).unwrap();
    session.finish().unwrap();

    assert!(
      matches!(opened.restore(&session_id), Err(StoreError::Invalid(message)) if message.contains("no semantic projection")),
      "a trace-backed message without a projection must fail closed"
    );
  }

  #[test]
  fn restore_rejects_an_orphan_linked_projection_record() {
    let tmp = TempDir::new("store-projection-orphan");
    let opened = store(&tmp);
    let session_id = SessionId::new();
    let turn_id = TurnId::new();
    let mut session = opened.begin(header(&session_id)).unwrap();
    let mut envelope = EventEnvelope::new(
      meta(&session_id, &turn_id),
      AgentEvent::UserMessage(UserMessage {
        text: "real".into(),
        attachments: 0,
      }),
    );
    session.emit(&mut envelope).unwrap();
    session
      .append_message(
        &turn_id,
        &Message::user("real"),
        0,
        &ModelRef::new("local", "qwen"),
        &envelope,
      )
      .unwrap();
    session
      .record(&SessionRecord::Message(SessionMessage {
        turn_id,
        role: Role::User,
        message: Message::user("orphan"),
        epoch: 0,
        model: ModelRef::new("local", "qwen"),
        event_id: EventId::new(),
        seq: Some(EventSeq(2)),
        external_context: None,
      }))
      .unwrap();
    session.finish().unwrap();

    assert!(
      matches!(opened.restore(&session_id), Err(StoreError::Invalid(message)) if message.contains("no canonical trace")),
      "an orphan linked projection must fail closed"
    );
  }

  #[test]
  fn resume_aborts_an_incomplete_compaction_and_keeps_the_old_context() {
    let tmp = TempDir::new("store-incomplete-compaction");
    let opened = store(&tmp);
    let session_id = SessionId::new();
    let turn_id = TurnId::new();
    let mut session = opened.begin(header(&session_id)).unwrap();
    session
      .emit(&mut EventEnvelope::new(
        meta(&session_id, &turn_id),
        AgentEvent::ContextCompactionStarted(rupi_core::ContextCompactionStarted {
          level: rupi_core::ContextLevel::L1Ordinary,
          reason: "crash before summary".into(),
        }),
      ))
      .unwrap();
    session.finish().unwrap();

    assert!(
      matches!(opened.restore(&session_id), Err(StoreError::Invalid(message)) if message.contains("incomplete context-compaction lifecycle")),
      "read-only restore must not mutate an incomplete lifecycle"
    );
    opened
      .resume(&session_id)
      .expect("writer recovery aborts the incomplete compaction")
      .finish()
      .unwrap();
    let restored = opened.restore(&session_id).unwrap();
    assert!(restored.messages.is_empty(), "old context remains empty");
    let trace = TraceJournal::read(&opened.layout().trace_path(&session_id)).unwrap();
    assert!(trace.items.iter().any(|entry| {
      matches!(
        &entry.envelope.event,
        AgentEvent::Diagnostic(diagnostic)
          if diagnostic.message.contains("recovered aborted context compaction")
      )
    }));
  }

  #[test]
  fn resume_discards_a_staged_compaction_summary_from_model_context() {
    let tmp = TempDir::new("store-incomplete-compaction-summary");
    let opened = store(&tmp);
    let session_id = SessionId::new();
    let turn_id = TurnId::new();
    let model = ModelRef::new("local", "qwen");
    let mut session = opened.begin(header(&session_id)).unwrap();
    let mut old = EventEnvelope::new(
      meta(&session_id, &turn_id),
      AgentEvent::UserMessage(UserMessage {
        text: "old context".into(),
        attachments: 0,
      }),
    );
    session.emit(&mut old).unwrap();
    session
      .append_message(&turn_id, &Message::user("old context"), 0, &model, &old)
      .unwrap();
    let mut start = EventEnvelope::new(
      meta(&session_id, &turn_id),
      AgentEvent::ContextCompactionStarted(rupi_core::ContextCompactionStarted {
        level: rupi_core::ContextLevel::L2Phase,
        reason: "crash after summary projection".into(),
      }),
    );
    session.emit(&mut start).unwrap();
    let mut summary = EventEnvelope::new(meta(&session_id, &turn_id), AgentEvent::ContextSummary);
    session.emit(&mut summary).unwrap();
    session
      .append_message(
        &turn_id,
        &Message::user("staged summary"),
        0,
        &model,
        &summary,
      )
      .unwrap();
    session.finish().unwrap();

    opened
      .resume(&session_id)
      .expect("resume aborts the staged summary")
      .finish()
      .unwrap();
    let restored = opened.restore(&session_id).unwrap();
    assert_eq!(
      restored.messages.len(),
      1,
      "the pre-compaction context remains model-visible"
    );
    assert_eq!(restored.messages[0].message.text(), "old context");
  }

  #[test]
  fn resume_synthesizes_a_missing_checkpoint_completion() {
    let tmp = TempDir::new("store-incomplete-checkpoint");
    let opened = store(&tmp);
    let session_id = SessionId::new();
    let turn_id = TurnId::new();
    let mut session = opened.begin(header(&session_id)).unwrap();
    let mut barrier = session
      .prepare_checkpoint(&capsule("checkpoint recovery"))
      .unwrap();
    barrier.context_epoch = 1;
    let created = rupi_core::CheckpointCreated {
      checkpoint_id: barrier.checkpoint_id.clone(),
      capsule_version: barrier.capsule_version,
      summarized_events: 0,
      path: barrier.capsule_path.clone(),
      context_epoch: barrier.context_epoch,
    };
    let mut envelope = EventEnvelope::new(
      meta(&session_id, &turn_id),
      AgentEvent::CheckpointCreated(created),
    );
    session
      .emit_transaction(
        &mut envelope,
        Some(SessionRecord::CheckpointBarrier(barrier)),
        false,
      )
      .unwrap();
    session.finish().unwrap();

    assert!(
      matches!(opened.restore(&session_id), Err(StoreError::Invalid(message)) if message.contains("checkpoint has no completion")),
      "read-only restore must not synthesize a lifecycle"
    );
    opened
      .resume(&session_id)
      .expect("resume closes the deterministic checkpoint boundary")
      .finish()
      .unwrap();
    let restored = opened.restore(&session_id).unwrap();
    assert!(restored.checkpoint.is_some());
    assert!(restored.compactions.iter().any(|compaction| {
      compaction.level == rupi_core::ContextLevel::L3Checkpoint
        && compaction.context_epoch == 1
        && compaction.retained_messages == 1
    }));
  }

  #[test]
  fn resume_synthesizes_a_prefix_checkpoint_completion_with_the_live_suffix() {
    let tmp = TempDir::new("store-prefix-checkpoint-recovery");
    let opened = store(&tmp);
    let session_id = SessionId::new();
    let turn_id = TurnId::new();
    let model = ModelRef::new("local", "qwen");
    let mut session = opened.begin(header(&session_id)).unwrap();

    for text in ["old one", "old two", "current-turn suffix"] {
      let mut event = EventEnvelope::new(
        meta(&session_id, &turn_id),
        AgentEvent::UserMessage(UserMessage {
          text: text.into(),
          attachments: 0,
        }),
      );
      session.emit(&mut event).unwrap();
      session
        .append_message(&turn_id, &Message::user(text), 0, &model, &event)
        .unwrap();
    }

    let mut barrier = session
      .prepare_checkpoint(&capsule("prefix checkpoint recovery"))
      .unwrap();
    barrier.context_epoch = 1;
    let created = CheckpointCreated {
      checkpoint_id: barrier.checkpoint_id.clone(),
      capsule_version: barrier.capsule_version,
      summarized_events: 2,
      path: barrier.capsule_path.clone(),
      context_epoch: barrier.context_epoch,
    };
    let mut checkpoint = EventEnvelope::new(
      meta(&session_id, &turn_id),
      AgentEvent::CheckpointCreated(created),
    );
    session
      .emit_transaction(
        &mut checkpoint,
        Some(SessionRecord::CheckpointBarrier(barrier)),
        false,
      )
      .unwrap();
    session.finish().unwrap();

    assert!(
      matches!(
        opened.restore(&session_id),
        Err(StoreError::Invalid(message)) if message.contains("checkpoint has no completion")
      ),
      "read-only restore must not synthesize a lifecycle"
    );
    opened
      .resume(&session_id)
      .expect("resume closes the prefix checkpoint boundary")
      .finish()
      .unwrap();

    let restored = opened.restore(&session_id).unwrap();
    assert_eq!(restored.messages.len(), 1);
    assert_eq!(restored.messages[0].message.text(), "current-turn suffix");
    assert!(restored.compactions.iter().any(|compaction| {
      compaction.level == rupi_core::ContextLevel::L3Checkpoint
        && compaction.context_epoch == 1
        && compaction.removed_messages == 2
        && compaction.retained_messages == 2
    }));
  }

  #[test]
  fn resume_reconstructs_a_checkpoint_barrier_from_a_prepare_only_wal() {
    let tmp = TempDir::new("store-checkpoint-prepare-only");
    let opened = store(&tmp);
    let session_id = SessionId::new();
    let turn_id = TurnId::new();
    let mut session = opened.begin(header(&session_id)).unwrap();
    let mut barrier = session
      .prepare_checkpoint(&capsule("prepare-only checkpoint"))
      .unwrap();
    barrier.context_epoch = 1;
    let created = CheckpointCreated {
      checkpoint_id: barrier.checkpoint_id.clone(),
      capsule_version: barrier.capsule_version,
      summarized_events: 0,
      path: barrier.capsule_path.clone(),
      context_epoch: barrier.context_epoch,
    };
    let mut checkpoint = EventEnvelope::new(
      meta(&session_id, &turn_id),
      AgentEvent::CheckpointCreated(created),
    );
    // Simulate a stop after the canonical append but before the WAL projection
    // line: the prepare is durable, while `intent.record` remains absent.
    session.wal.prepare(&checkpoint).unwrap();
    session.emit(&mut checkpoint).unwrap();
    // Drop simulates a crash: a clean finish intentionally refuses to close
    // this prepare-only transaction because recovery must own the decision.
    drop(session);

    opened
      .resume(&session_id)
      .expect("resume reconstructs the missing checkpoint barrier")
      .finish()
      .unwrap();
    let restored = opened.restore(&session_id).unwrap();
    assert!(restored.checkpoint.is_some());
    assert!(restored.compactions.iter().any(|compaction| {
      compaction.level == rupi_core::ContextLevel::L3Checkpoint
        && compaction.context_epoch == 1
        && compaction.retained_messages == 1
    }));
  }

  #[test]
  fn resume_completes_a_compaction_after_its_abort_diagnostic_was_durable() {
    let tmp = TempDir::new("store-compaction-abort-diagnostic");
    let opened = store(&tmp);
    let session_id = SessionId::new();
    let turn_id = TurnId::new();
    let mut session = opened.begin(header(&session_id)).unwrap();
    let mut start = EventEnvelope::new(
      meta(&session_id, &turn_id),
      AgentEvent::ContextCompactionStarted(rupi_core::ContextCompactionStarted {
        level: rupi_core::ContextLevel::L1Ordinary,
        reason: "crash after abort diagnostic".into(),
      }),
    );
    session.emit_transaction(&mut start, None, true).unwrap();
    let mut summary = EventEnvelope::new(meta(&session_id, &turn_id), AgentEvent::ContextSummary);
    session.emit_transaction(&mut summary, None, true).unwrap();
    let mut diagnostic = EventEnvelope::new(
      meta(&session_id, &turn_id),
      AgentEvent::Diagnostic(Diagnostic {
        level: DiagnosticLevel::Warn,
        message: compaction_abort_message(&start.meta.event_id),
      }),
    );
    diagnostic.meta.parent_event_id = Some(start.meta.event_id.clone());
    session.emit(&mut diagnostic).unwrap();
    // The held compaction intent is intentionally left open; the next writer
    // must notice the already durable abort diagnostic and add its marker.
    drop(session);

    opened
      .resume(&session_id)
      .expect("resume appends the missing abort projection marker")
      .finish()
      .unwrap();
    let restored = opened.restore(&session_id).unwrap();
    assert!(restored.messages.is_empty());
    assert!(
      restored
        .compactions
        .iter()
        .any(|compaction| compaction.aborted)
    );
  }

  #[test]
  fn restore_reports_tool_requests_without_terminal_lifecycle_events() {
    let tmp = TempDir::new("store-interrupted-tool");
    let opened = store(&tmp);
    let session_id = SessionId::new();
    let turn_id = TurnId::new();
    let mut session = opened.begin(header(&session_id)).unwrap();
    let call_id = ToolCallId::new();
    let mut requested = EventEnvelope::new(
      meta(&session_id, &turn_id),
      AgentEvent::ToolRequested(ToolRequested {
        call_id: call_id.clone(),
        name: "write".into(),
        arguments: serde_json::json!({"path":"a.txt", "content":"x"}),
        read_only: false,
      }),
    );
    session.emit(&mut requested).unwrap();
    let mut started = EventEnvelope::new(
      meta(&session_id, &turn_id),
      AgentEvent::ToolStarted(ToolStarted {
        call_id,
        name: "write".into(),
      }),
    );
    session.emit(&mut started).unwrap();
    session.finish().unwrap();

    let restored = opened.restore(&session_id).unwrap();
    assert_eq!(restored.interrupted_tools.len(), 1);
    assert_eq!(
      restored.interrupted_tools[0].state,
      rupi_core::ToolExecutionState::Started
    );
    assert_eq!(restored.interrupted_tools[0].request.name, "write");
  }

  #[test]
  fn recovery_scopes_parentless_legacy_call_ids_to_active_invocations() {
    let tmp = TempDir::new("store-reused-tool-id");
    let opened = store(&tmp);
    let session_id = SessionId::new();
    let call_id = ToolCallId::from_string("call_1");
    let mut session = opened.begin(header(&session_id)).unwrap();
    for _ in 0..2 {
      let turn_id = TurnId::new();
      session
        .emit(&mut EventEnvelope::new(
          meta(&session_id, &turn_id),
          AgentEvent::ToolRequested(ToolRequested {
            call_id: call_id.clone(),
            name: "read".into(),
            arguments: serde_json::json!({}),
            read_only: true,
          }),
        ))
        .unwrap();
      session
        .emit(&mut EventEnvelope::new(
          meta(&session_id, &turn_id),
          AgentEvent::ToolStarted(ToolStarted {
            call_id: call_id.clone(),
            name: "read".into(),
          }),
        ))
        .unwrap();
      session
        .emit(&mut EventEnvelope::new(
          meta(&session_id, &turn_id),
          AgentEvent::ToolCompleted(ToolCompleted {
            call_id: call_id.clone(),
            name: "read".into(),
            state: ToolExecutionState::Succeeded,
            duration_ms: 0,
            status: Some(0),
            reduced: false,
            blob: None,
            visible_bytes: 0,
          }),
        ))
        .unwrap();
    }
    let trace_path = session.trace_path().to_path_buf();
    session.finish().unwrap();
    let trace = TraceJournal::read(&trace_path).unwrap();
    assert!(
      scan_tool_lifecycles(&trace.items).unwrap().is_empty(),
      "a completed legacy invocation must not reserve its provider id forever"
    );
  }

  #[test]
  fn restore_refuses_a_tool_request_that_never_started() {
    let tmp = TempDir::new("store-unstarted-tool");
    let opened = store(&tmp);
    let session_id = SessionId::new();
    let turn_id = TurnId::new();
    let mut session = opened.begin(header(&session_id)).unwrap();
    session
      .emit(&mut EventEnvelope::new(
        meta(&session_id, &turn_id),
        AgentEvent::ToolRequested(ToolRequested {
          call_id: ToolCallId::new(),
          name: "write".into(),
          arguments: serde_json::json!({"path":"a.txt", "content":"x"}),
          read_only: false,
        }),
      ))
      .unwrap();
    session.finish().unwrap();

    let error = opened
      .restore(&session_id)
      .expect_err("an unstarted durable request cannot be replayed safely");
    assert!(
      matches!(&error, StoreError::Invalid(message) if message.contains("no ToolStarted")),
      "resume must fail closed: {error}"
    );
  }

  #[test]
  fn restore_refuses_orphan_duplicate_and_out_of_order_tool_lifecycle_events() {
    for (label, events) in [
      (
        "orphan-start",
        vec![AgentEvent::ToolStarted(ToolStarted {
          call_id: ToolCallId::new(),
          name: "read".into(),
        })],
      ),
      ("duplicate-start", {
        let call_id = ToolCallId::new();
        vec![
          AgentEvent::ToolRequested(ToolRequested {
            call_id: call_id.clone(),
            name: "read".into(),
            arguments: serde_json::json!({}),
            read_only: true,
          }),
          AgentEvent::ToolStarted(ToolStarted {
            call_id: call_id.clone(),
            name: "read".into(),
          }),
          AgentEvent::ToolStarted(ToolStarted {
            call_id,
            name: "read".into(),
          }),
        ]
      }),
      ("success-before-start", {
        let call_id = ToolCallId::new();
        vec![
          AgentEvent::ToolRequested(ToolRequested {
            call_id: call_id.clone(),
            name: "read".into(),
            arguments: serde_json::json!({}),
            read_only: true,
          }),
          AgentEvent::ToolCompleted(ToolCompleted {
            call_id,
            name: "read".into(),
            state: ToolExecutionState::Succeeded,
            duration_ms: 0,
            status: Some(0),
            reduced: false,
            blob: None,
            visible_bytes: 0,
          }),
        ]
      }),
    ] {
      let tmp = TempDir::new(&format!("store-tool-{label}"));
      let opened = store(&tmp);
      let session_id = SessionId::new();
      let turn_id = TurnId::new();
      let mut session = opened.begin(header(&session_id)).unwrap();
      for event in events {
        session
          .emit(&mut EventEnvelope::new(meta(&session_id, &turn_id), event))
          .unwrap();
      }
      session.finish().unwrap();
      assert!(
        matches!(opened.restore(&session_id), Err(StoreError::Invalid(_))),
        "{label} lifecycle must fail closed"
      );
    }
  }

  #[test]
  fn a_second_resume_is_refused_while_the_first_session_handle_is_open() {
    let tmp = TempDir::new("store-lease");
    let opened = store(&tmp);
    let session_id = SessionId::new();
    let first = opened.begin(header(&session_id)).unwrap();
    let error = opened.resume(&session_id).unwrap_err();
    assert!(matches!(error, StoreError::Invalid(_)), "{error}");
    first.finish().unwrap();
    opened.resume(&session_id).unwrap().finish().unwrap();
  }

  #[test]
  fn retention_skips_a_live_session_lease() {
    let tmp = TempDir::new("store-retention-lease");
    let opened = store(&tmp);
    let session_id = SessionId::new();
    let session = opened.begin(header(&session_id)).unwrap();
    let report = opened
      .apply_retention(
        &TraceRetention {
          max_age_days: Some(0),
          ..TraceRetention::default()
        },
        u64::MAX,
        0,
      )
      .unwrap();
    assert_eq!(report.leased, 1);
    assert!(opened.exists(&session_id));
    session.finish().unwrap();
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
