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

use pi_rs_core::{
  capability::ModelRef,
  context::{ContextCapsule, ExternalContextRef},
  event::{AgentEvent, EventEnvelope},
  ids::{CheckpointId, EventId, EventSeq, SessionId, ToolCallId, TurnId},
  message::{Message, Role},
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
  projection::{ProjectionWal, validate_projection_size},
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
    // Recovery may have repaired a WAL intent; validate the complete state
    // before returning an append handle so callers cannot issue a provider
    // request from a projection that still disagrees with canonical history.
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
    }
  }

  /// Read what is needed to continue, without opening writers.
  ///
  /// Resume cost is bounded by the checkpoint barrier, not by total session
  /// length: messages before the latest barrier are summarized inside the
  /// capsule, so reading them would be pure waste.
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

  /// Complete the pending event transaction with the model-visible message.
  ///
  /// A missing pending intent is retained as a compatibility fallback for
  /// callers that use the low-level `emit`/`append_message` pair directly.
  pub fn complete_message(
    &mut self,
    attributed: &pi_rs_core::AttributedMessage,
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
    // Message payloads may be arbitrarily large (the journal can externalize
    // their trace fields), so do not duplicate them in the WAL. The prepare
    // intent plus canonical event are enough to recover a missing append; when
    // the semantic append already reached disk, recovery finds it by event id
    // and only closes the intent.
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
    std::fs::write(&temporary, &encoded)?;
    std::fs::rename(&temporary, &path)?;
    std::fs::File::open(&path)?.sync_data()?;

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

  /// Reconcile uncommitted trace/session projection intents before a resumed
  /// runtime is allowed to issue a provider request.
  fn recover_projection(&mut self) -> Result<(), StoreError> {
    let pending = self.wal.pending()?;
    if pending.is_empty() {
      return Ok(());
    }
    let trace_report = TraceJournal::read(self.trace_path())?;
    if trace_report.malformed > 0 {
      return Err(StoreError::Invalid(format!(
        "session {} trace contains {} malformed line(s); projection recovery is unsafe",
        self.id(),
        trace_report.malformed
      )));
    }
    validate_trace_integrity(&trace_report.items, self.id())?;
    validate_model_request_lifecycles(&trace_report.items, self.id())?;
    validate_compaction_lifecycles(&trace_report.items, self.id())?;
    validate_checkpoint_lifecycles(&trace_report.items, self.id())?;
    let trace = trace_report.items;
    let mut records = SessionLog::read(self.path())?.items;
    let mut pending = pending;
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
                if completed.level != pi_rs_core::ContextLevel::L3Checkpoint
            )
        });
        if !completed {
          return Err(StoreError::Invalid(format!(
            "session {} has an incomplete context-compaction lifecycle; resume requires recovery",
            self.id()
          )));
        }
        self.wal.commit(&intent.tx_id)?;
        continue;
      }

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
          recover_projection_record(trace_entry, &trace, &records, self)?
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
      if recover_projection_record(trace_entry, &trace_report.items, &[], self)?.is_none() {
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
    if capsule.version != pi_rs_core::context::CAPSULE_SCHEMA_VERSION
      || barrier.capsule_version != pi_rs_core::context::CAPSULE_SCHEMA_VERSION
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
  entries: &[pi_rs_core::TraceEntry],
  session: &SessionId,
) -> Result<(), StoreError> {
  let mut last_seq = None;
  let mut event_ids = BTreeSet::new();
  for entry in entries {
    if entry.envelope.v != pi_rs_core::event::EVENT_SCHEMA_VERSION {
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
  entries: &[pi_rs_core::TraceEntry],
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

fn validate_model_request_lifecycles(
  entries: &[pi_rs_core::TraceEntry],
  session: &SessionId,
) -> Result<(), StoreError> {
  let mut open: Vec<(Option<TurnId>, u32, ModelRef)> = Vec::new();
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
        if open.iter().any(|candidate| candidate == &key) {
          return Err(StoreError::Invalid(format!(
            "session {session} has overlapping model request lifecycles; resume requires recovery"
          )));
        }
        open.push(key);
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
        let Some(index) = open.iter().rposition(|candidate| *candidate == key) else {
          return Err(StoreError::Invalid(format!(
            "session {session} has a model request completion without a matching start; resume requires recovery"
          )));
        };
        open.remove(index);
      }
      _ => {}
    }
  }
  if open.is_empty() {
    return Ok(());
  }
  Err(StoreError::Invalid(format!(
    "session {session} trace contains an incomplete model request lifecycle; resume requires recovery"
  )))
}

fn validate_compaction_lifecycles(
  entries: &[pi_rs_core::TraceEntry],
  session: &SessionId,
) -> Result<(), StoreError> {
  #[derive(Debug)]
  struct OpenCompaction {
    index: usize,
    level: pi_rs_core::ContextLevel,
    turn_id: Option<TurnId>,
  }

  let mut open: Option<OpenCompaction> = None;
  for (index, entry) in entries.iter().enumerate() {
    match &entry.envelope.event {
      AgentEvent::ContextCompactionStarted(started) => {
        if started.level == pi_rs_core::ContextLevel::L3Checkpoint {
          return Err(StoreError::Invalid(format!(
            "session {session} has a checkpoint compaction start without a checkpoint barrier; resume requires recovery"
          )));
        }
        if started.level == pi_rs_core::ContextLevel::L0Payload || open.is_some() {
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
        if completed.level != pi_rs_core::ContextLevel::L3Checkpoint =>
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
  entries: &[pi_rs_core::TraceEntry],
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
        if completed.level == pi_rs_core::ContextLevel::L3Checkpoint
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
  entries: &[pi_rs_core::TraceEntry],
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
  let scoped_compactions: Vec<&pi_rs_core::SessionCompactionRecord> = records
    .iter()
    .filter_map(|record| match record {
      SessionRecord::Compaction(compaction)
        if compaction.replaces_from.is_some() && compaction.replaces_through.is_some() =>
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
  let fail = |entry: &pi_rs_core::TraceEntry| {
    StoreError::Invalid(format!(
      "session {session} canonical {} event {} has no semantic projection; resume requires recovery",
      event_kind(&entry.envelope.event),
      entry.envelope.meta.event_id
    ))
  };

  for entry in entries {
    // A checkpoint changes the model-visible window, not the durable joins. All
    // message and lifecycle facts remain auditable against their projections.
    match &entry.envelope.event {
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
      AgentEvent::ModelRequestCompleted(completed) if completed.finish_reason.is_some() => {
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
                  && matches!(&candidate.envelope.event, AgentEvent::AssistantDelta(_))
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
        pi_rs_core::ContentBlock::ToolCall(call) => Some(call),
        _ => None,
      })
    {
      let requested = entries.iter().any(|entry| {
        matches!(
          &entry.envelope.event,
          AgentEvent::ToolRequested(requested)
            if requested.call_id == call.id
              && requested.name == call.name
              && requested.arguments == call.arguments
              && entry.envelope.meta.turn_id == Some(message.turn_id.clone())
        )
      });
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
    if !compaction.summary_present || compaction.level == pi_rs_core::ContextLevel::L3Checkpoint {
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
  entries: &[pi_rs_core::TraceEntry],
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
  trace_entry: &pi_rs_core::TraceEntry,
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
          .filter(|block| matches!(block, pi_rs_core::ContentBlock::Image { .. }))
          .count();
        let valid = message.message.content.iter().all(|block| {
          matches!(
            block,
            pi_rs_core::ContentBlock::Text { .. } | pi_rs_core::ContentBlock::Image { .. }
          )
        }) && message.message.text() == user.text
          // `attachments` also counts imported block kinds that pi-rs cannot
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
          Some(pi_rs_core::ContentBlock::Text { .. })
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
          Some(pi_rs_core::ContentBlock::Text { .. })
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
        .filter(|block| matches!(block, pi_rs_core::ContentBlock::ToolCall(_)))
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
      let pi_rs_core::ContentBlock::ToolResult(result) = &message.message.content[0] else {
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
  let pi_rs_core::ContentBlock::ToolResult(result) = &message.message.content[0] else {
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

fn tool_meta_matches(call: &InterruptedToolCall, entry: &pi_rs_core::TraceEntry) -> bool {
  call.turn_id == entry.envelope.meta.turn_id
    && call
      .epoch
      .is_none_or(|epoch| entry.envelope.meta.model_epoch == Some(epoch))
    && call
      .model
      .as_ref()
      .is_none_or(|model| entry.envelope.meta.model.as_ref() == Some(model))
    && entry
      .envelope
      .meta
      .tool_call_id
      .as_ref()
      .is_none_or(|call_id| call_id == &call.request.call_id)
}

fn interrupted_tool_calls(
  entries: &[pi_rs_core::TraceEntry],
) -> Result<Vec<InterruptedToolCall>, StoreError> {
  let mut calls = BTreeMap::<ToolCallId, InterruptedToolCall>::new();
  let mut seen = BTreeSet::<ToolCallId>::new();
  let mut ordered = Vec::<ToolCallId>::new();
  let invalid = |entry: &pi_rs_core::TraceEntry, detail: &str| {
    StoreError::Invalid(format!(
      "session {} has an invalid tool lifecycle at {}: {detail}; resume requires recovery",
      entry.envelope.meta.session_id, entry.envelope.meta.event_id
    ))
  };

  for entry in entries {
    match &entry.envelope.event {
      AgentEvent::ToolRequested(requested) => {
        if entry.envelope.meta.turn_id.is_none() || requested.call_id.as_str().is_empty() {
          return Err(invalid(entry, "tool request has no turn or call identity"));
        }
        if !seen.insert(requested.call_id.clone()) {
          return Err(invalid(entry, "duplicate tool request"));
        }
        let request = ToolRequest {
          call_id: requested.call_id.clone(),
          name: requested.name.clone(),
          arguments: requested.arguments.clone(),
        };
        calls.insert(
          requested.call_id.clone(),
          InterruptedToolCall {
            request,
            state: ToolExecutionState::Requested,
            read_only: requested.read_only,
            turn_id: entry.envelope.meta.turn_id.clone(),
            epoch: entry.envelope.meta.model_epoch,
            model: entry.envelope.meta.model.clone(),
          },
        );
        ordered.push(requested.call_id.clone());
      }
      AgentEvent::ToolStarted(started) => {
        let Some(call) = calls.get_mut(&started.call_id) else {
          return Err(invalid(entry, "tool started without a matching request"));
        };
        if call.state != ToolExecutionState::Requested {
          return Err(invalid(entry, "duplicate tool start"));
        }
        if started.name != call.request.name || !tool_meta_matches(call, entry) {
          return Err(invalid(entry, "tool start disagrees with its request"));
        }
        call.state = ToolExecutionState::Started;
      }
      AgentEvent::ToolCompleted(completed) => {
        let Some(call) = calls.remove(&completed.call_id) else {
          return Err(invalid(entry, "tool completed without a matching request"));
        };
        if call.state != ToolExecutionState::Started {
          return Err(invalid(entry, "tool completed before it started"));
        }
        if completed.name != call.request.name || !tool_meta_matches(&call, entry) {
          return Err(invalid(entry, "tool completion disagrees with its request"));
        }
        if completed.state != ToolExecutionState::Succeeded {
          return Err(invalid(entry, "tool completed with a non-success state"));
        }
      }
      AgentEvent::ToolFailed(failed) => {
        let Some(call) = calls.remove(&failed.call_id) else {
          return Err(invalid(entry, "tool failed without a matching request"));
        };
        // A failed request can be an explicit no-execution terminal result:
        // the runtime records this when cancellation/provider failure occurs
        // before it can invoke the tool. It is safe to close Requested here;
        // an unclosed request remains a hard resume refusal below.
        if call.state != ToolExecutionState::Requested && call.state != ToolExecutionState::Started
        {
          return Err(invalid(
            entry,
            "tool failed from an invalid lifecycle state",
          ));
        }
        if failed.name != call.request.name || !tool_meta_matches(&call, entry) {
          return Err(invalid(entry, "tool failure disagrees with its request"));
        }
      }
      AgentEvent::ToolUnknown(unknown) => {
        let Some(call) = calls.remove(&unknown.call_id) else {
          return Err(invalid(
            entry,
            "tool became unknown without a matching request",
          ));
        };
        // `Unknown` is also the explicit terminal used for a refused or
        // cancelled request that never crossed the execution boundary.
        if call.state != ToolExecutionState::Requested && call.state != ToolExecutionState::Started
        {
          return Err(invalid(
            entry,
            "tool became unknown from an invalid lifecycle state",
          ));
        }
        if unknown.mutating == call.read_only || !tool_meta_matches(&call, entry) {
          return Err(invalid(
            entry,
            "tool unknown result disagrees with its request",
          ));
        }
      }
      _ => {}
    }
  }

  let mut interrupted = Vec::new();
  for call_id in ordered {
    let Some(call) = calls.remove(&call_id) else {
      continue;
    };
    if call.state == ToolExecutionState::Requested {
      return Err(StoreError::Invalid(format!(
        "durable tool request {call_id} has no ToolStarted; resume requires recovery"
      )));
    }
    // A started call is deliberately returned as `Unknown` work for runtime
    // reconciliation; it must never be replayed as if execution were certain.
    interrupted.push(call);
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
  trace_entry: &pi_rs_core::TraceEntry,
  session: &Session,
) -> Result<AgentEvent, StoreError> {
  restore_externalized_event_from_blobs(trace_entry, session.blobs(), session.id())
}

fn restore_externalized_event_from_blobs(
  trace_entry: &pi_rs_core::TraceEntry,
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

fn recover_projection_record(
  trace_entry: &pi_rs_core::TraceEntry,
  trace: &[pi_rs_core::TraceEntry],
  records: &[SessionRecord],
  session: &Session,
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
    AgentEvent::ContextSummary
    | AgentEvent::ToolCompleted(_)
    | AgentEvent::ToolFailed(_)
    | AgentEvent::ToolUnknown(_) => Err(StoreError::Invalid(format!(
      "session {} has an interrupted message projection for {}; resume requires manual recovery",
      session.id(),
      trace_entry.envelope.meta.event_id
    ))),
    AgentEvent::ModelRequestCompleted(completed) => {
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
      Ok(Some(SessionRecord::Epoch(pi_rs_core::SessionEpochRecord {
        epoch: started.epoch,
        model: started.model.clone(),
        reason: started.reason.clone(),
      })))
    }
    AgentEvent::ContextCompactionCompleted(completed) => {
      let (summary_present, range) = if completed.level == pi_rs_core::ContextLevel::L3Checkpoint {
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
        pi_rs_core::SessionCompactionRecord {
          context_epoch: completed.context_epoch,
          level: completed.level,
          removed_messages: completed.removed_messages,
          retained_from: 0,
          retained_messages: completed.retained_messages,
          summary_present,
          replaces_from: range.map(|(from, _)| from),
          replaces_through: range.map(|(_, through)| through),
        },
      )))
    }
    AgentEvent::ContextReduced(reduced) if reduced.removed_messages > 0 => Ok(Some(
      SessionRecord::Reduction(pi_rs_core::SessionReductionRecord {
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
  use pi_rs_core::{
    capability::EpochReason,
    context::CAPSULE_SCHEMA_VERSION,
    event::{
      AgentEvent, CheckpointCreated, Diagnostic, DiagnosticLevel, EventMeta, SessionEndReason,
      SessionEnded, SessionStarted, ToolCompleted, ToolRequested, ToolStarted, TurnCompleted,
      TurnStatus, UserMessage,
    },
    ids::{EventId, ToolCallId, TraceId, uuidv7},
    session::{SESSION_SCHEMA_VERSION, SessionEpochRecord},
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
        AgentEvent::ToolFailed(pi_rs_core::ToolFailed {
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
      AgentEvent::ModelRequestStarted(pi_rs_core::ModelRequestStarted {
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
      AgentEvent::AssistantDelta(pi_rs_core::AssistantDelta {
        text: "only this answer".into(),
        chunk_index: 0,
      }),
    );
    session.emit(&mut first_delta).unwrap();
    session
      .emit(&mut EventEnvelope::new(
        meta(&session_id, &turn_id),
        AgentEvent::ModelRequestCompleted(pi_rs_core::ModelRequestCompleted {
          epoch: 0,
          model: ModelRef::new("local", "qwen"),
          finish_reason: None,
          input_tokens: None,
          output_tokens: None,
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
      AgentEvent::ModelRequestStarted(pi_rs_core::ModelRequestStarted {
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
      AgentEvent::AssistantDelta(pi_rs_core::AssistantDelta {
        text: "must not merge".into(),
        chunk_index: 0,
      }),
    );
    session.emit(&mut prior_delta).unwrap();
    let mut completion = EventEnvelope::new(
      meta(&session_id, &turn_id),
      AgentEvent::ModelRequestCompleted(pi_rs_core::ModelRequestCompleted {
        epoch: 0,
        model: ModelRef::new("local", "qwen"),
        finish_reason: Some("stop".into()),
        input_tokens: Some(1),
        output_tokens: Some(3),
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
        AgentEvent::ContextCompactionStarted(pi_rs_core::ContextCompactionStarted {
          level: pi_rs_core::ContextLevel::L1Ordinary,
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
      AgentEvent::ContextCompactionEpoch(pi_rs_core::ContextCompactionEpoch {
        context_epoch: 1,
        replaces_from: messages[0].meta.seq.unwrap(),
        replaces_through: messages[1].meta.seq.unwrap(),
        summary: None,
      }),
    );
    session.emit(&mut epoch).unwrap();
    let mut completion = EventEnvelope::new(
      meta(&session_id, &turn_id),
      AgentEvent::ContextCompactionCompleted(pi_rs_core::ContextCompactionCompleted {
        level: pi_rs_core::ContextLevel::L1Ordinary,
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
          capabilities: pi_rs_core::ModelCapabilities::text_only(8_192),
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
  fn restore_refuses_an_incomplete_compaction_lifecycle() {
    let tmp = TempDir::new("store-incomplete-compaction");
    let opened = store(&tmp);
    let session_id = SessionId::new();
    let turn_id = TurnId::new();
    let mut session = opened.begin(header(&session_id)).unwrap();
    session
      .emit(&mut EventEnvelope::new(
        meta(&session_id, &turn_id),
        AgentEvent::ContextCompactionStarted(pi_rs_core::ContextCompactionStarted {
          level: pi_rs_core::ContextLevel::L1Ordinary,
          reason: "crash before summary".into(),
        }),
      ))
      .unwrap();
    session.finish().unwrap();

    assert!(
      matches!(opened.restore(&session_id), Err(StoreError::Invalid(message)) if message.contains("incomplete context-compaction lifecycle")),
      "an open compaction cannot be resumed with ambiguous history"
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
      pi_rs_core::ToolExecutionState::Started
    );
    assert_eq!(restored.interrupted_tools[0].request.name, "write");
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
