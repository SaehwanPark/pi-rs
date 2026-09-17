//! Semantic session log (`sessions/<id>.jsonl`).
//!
//! The session log is what the runtime needs in order to continue: the header,
//! the messages with model attribution, epoch records, compaction markers, and
//! checkpoint barriers. It is a projection of the canonical trace, written so
//! that model-visible reconstruction does not need to hydrate the trace; the
//! store still scans canonical history to validate integrity and uncertainty.
//!
//! Every line is written durably. Unlike trace deltas, a lost message is not a
//! cosmetic loss: it silently changes what the next model request believes
//! happened. Bounding resume cost is the checkpoint barrier's job, not the
//! writer's. Every new header and semantic record is sanitized by the store's
//! configured redaction policy immediately before serialization.

use std::{collections::HashSet, path::Path};

use pi_rs_core::{
  capability::ModelRef,
  event::AgentEvent,
  ids::{EventSeq, SessionId},
  redact::RedactionPolicy,
  session::{
    SESSION_SCHEMA_VERSION, SessionHeader, SessionMessage, SessionRecord, SessionReductionRecord,
    SessionSummary,
  },
};

use crate::{
  StoreError,
  jsonl::{LineWriter, MAX_JSONL_LINE_BYTES, ReadReport, read_first_line, read_jsonl},
};

/// Bytes read from a trace tail to decide whether a session is closed.
const TAIL_WINDOW: u64 = 32 * 1024;

/// Refuse records that the bounded JSONL reader could not hydrate later.
fn ensure_line_bound(path: &Path, line: &str) -> Result<(), StoreError> {
  if line.len().saturating_add(1) > MAX_JSONL_LINE_BYTES {
    return Err(StoreError::Invalid(format!(
      "{} record exceeds the {}-byte JSONL line bound",
      path.display(),
      MAX_JSONL_LINE_BYTES
    )));
  }
  Ok(())
}

/// Writer and reader for one session's semantic state.
#[derive(Debug)]
pub struct SessionLog {
  writer: LineWriter,
  header: SessionHeader,
  records: usize,
  redaction: RedactionPolicy,
}

impl SessionLog {
  /// Create a session log and write its header.
  ///
  /// The header is written through immediately: a session that exists at all
  /// must be listable, even if the process dies before the first message.
  pub fn create(path: &Path, header: SessionHeader) -> Result<Self, StoreError> {
    Self::create_with_policy(path, header, RedactionPolicy::default())
  }

  /// Create a session log protected by the store's configured redaction policy.
  pub fn create_with_policy(
    path: &Path,
    header: SessionHeader,
    redaction: RedactionPolicy,
  ) -> Result<Self, StoreError> {
    if read_first_line(path)?.is_some() {
      return Err(StoreError::Invalid(format!(
        "session {} already exists; a session log is never truncated",
        path.display()
      )));
    }
    let sanitized = sanitize_record(&SessionRecord::Header(header), &redaction)?;
    let SessionRecord::Header(header) = sanitized else {
      unreachable!("sanitizing a header preserves its record variant")
    };
    let header_line = serde_json::to_string(&SessionRecord::Header(header.clone()))?;
    ensure_line_bound(path, &header_line)?;
    let mut writer = LineWriter::create(path)?;
    writer.write_line(&header_line, true)?;
    Ok(Self {
      writer,
      header,
      records: 1,
      redaction,
    })
  }

  /// Reopen an existing session log for appending.
  pub fn resume(path: &Path) -> Result<Self, StoreError> {
    Self::resume_with_policy(path, RedactionPolicy::default())
  }

  /// Resume a session using the currently configured redaction policy for new records.
  pub fn resume_with_policy(path: &Path, redaction: RedactionPolicy) -> Result<Self, StoreError> {
    let header = Self::read_header(path)?;
    let report = Self::read(path)?;
    if report.malformed > 0 {
      return Err(StoreError::Invalid(format!(
        "{} contains {} malformed record(s); resume requires recovery",
        path.display(),
        report.malformed
      )));
    }
    let records = report.items.len();
    Ok(Self {
      writer: LineWriter::create(path)?,
      header,
      records: records.max(1),
      redaction,
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
    let line = serde_json::to_string(&sanitize_record(record, &self.redaction)?)?;
    ensure_line_bound(self.path(), &line)?;
    self
      .records
      .checked_add(1)
      .ok_or_else(|| StoreError::Invalid("session record count is exhausted".into()))?;
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

  /// Read every record and enforce the header-selected schema compatibility.
  pub fn read(path: &Path) -> Result<ReadReport<SessionRecord>, StoreError> {
    let report = read_jsonl(path)?;
    validate_record_schema(path, &report.items)?;
    Ok(report)
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
          messages = messages
            .checked_add(1)
            .ok_or_else(|| StoreError::Invalid("session message count is exhausted".into()))?;
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

fn sanitize_record(
  record: &SessionRecord,
  policy: &RedactionPolicy,
) -> Result<SessionRecord, StoreError> {
  let mut value = serde_json::to_value(record)?;
  policy.apply_json(&mut value);
  serde_json::from_value(value).map_err(StoreError::from)
}

fn validate_record_schema(path: &Path, records: &[SessionRecord]) -> Result<(), StoreError> {
  let Some(SessionRecord::Header(header)) = records.first() else {
    return Ok(());
  };
  validate_header(header, path)?;
  if header.version < 2
    && records
      .iter()
      .any(|record| matches!(record, SessionRecord::Reduction(_)))
  {
    return Err(StoreError::Invalid(format!(
      "{} uses session schema version {} but contains a reduction record introduced in version 2; migrate before reading",
      path.display(),
      header.version
    )));
  }
  Ok(())
}

fn validate_header(header: &SessionHeader, path: &Path) -> Result<(), StoreError> {
  if header.version == 0 {
    return Err(StoreError::Invalid(format!(
      "{} uses unsupported session schema version 0",
      path.display()
    )));
  }
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
  /// Durable L0 history reductions applied to the model-visible projection.
  pub reductions: Vec<SessionReductionRecord>,
  /// Tool calls whose terminal lifecycle event was absent from the canonical trace.
  pub interrupted_tools: Vec<pi_rs_core::InterruptedToolCall>,
  /// Highest model-visible compaction epoch persisted in the session log.
  pub context_epoch: u32,
  /// Messages summarized by the checkpoint, for honest UI reporting.
  pub summarized_messages: usize,
  /// Sequence of the last record this session log knows about.
  pub last_seq: Option<EventSeq>,
  pub malformed_records: usize,
  pub total_records: usize,
}

/// Restore session state as `latest checkpoint + records after it`, applying
/// projected compaction markers to recover the exact model-visible window while
/// leaving canonical session history untouched on disk.
pub fn restore(path: &Path) -> Result<RestoredSession, StoreError> {
  let report = SessionLog::read(path)?;
  restore_from_report(path, &report)
}

/// Restore from a report the caller already loaded. Keeping this internal seam
/// avoids parsing the full semantic log a second time when `Store::restore`
/// also needs the records for trace/projection alignment.
pub(crate) fn restore_from_report(
  path: &Path,
  report: &crate::jsonl::ReadReport<SessionRecord>,
) -> Result<RestoredSession, StoreError> {
  let header = SessionLog::read_header(path)?;
  let mut messages: Vec<SessionMessage> = Vec::new();
  let mut epochs = Vec::new();
  let mut compactions = Vec::new();
  let mut reductions = Vec::new();
  let mut checkpoint = None;
  let mut checkpoint_seq = None;
  // A checkpoint barrier is written before its L3 completion. Keep the
  // pre-barrier projection until that completion tells us how many messages
  // were actually summarized; prefix checkpoints intentionally retain a
  // current-turn suffix that must survive the barrier.
  let mut checkpoint_pre_barrier: Option<Vec<SessionMessage>> = None;
  let mut context_epoch = 0u32;
  let mut summarized_messages = 0usize;
  let mut last_seq: Option<EventSeq> = None;
  let mut last_message_seq: Option<EventSeq> = None;
  let mut last_reduction_seq: Option<EventSeq> = None;
  let mut last_compaction_epoch: Option<u32> = None;
  let mut last_context_epoch: Option<u32> = None;
  let mut last_checkpoint_epoch: Option<u32> = None;
  let mut seen_checkpoint_ids = HashSet::new();
  let mut seen_event_ids = HashSet::new();
  for record in report.items.iter().skip(1) {
    match record {
      SessionRecord::Message(message) => {
        // A legacy/standalone barrier has no L3 marker to describe its
        // retained suffix. Seeing a later message closes that pending window;
        // a real prefix checkpoint is handled by the L3 branch below instead.
        if let Some(pre_barrier) = checkpoint_pre_barrier.take() {
          summarized_messages = summarized_messages
            .checked_add(pre_barrier.len())
            .ok_or_else(|| StoreError::Invalid("summarized message count is exhausted".into()))?;
        }
        if !seen_event_ids.insert(message.event_id.clone()) {
          return Err(StoreError::Invalid(format!(
            "{} contains duplicate semantic event {}",
            path.display(),
            message.event_id
          )));
        }
        if let Some(seq) = message.seq {
          if seq.0 == 0 {
            return Err(StoreError::Invalid(format!(
              "{} contains a zero message sequence at event {}",
              path.display(),
              message.event_id
            )));
          }
          if last_message_seq.is_some_and(|previous| seq <= previous)
            || last_reduction_seq.is_some_and(|previous| seq <= previous)
          {
            return Err(StoreError::Invalid(format!(
              "{} contains a non-monotonic message sequence at event {}",
              path.display(),
              message.event_id
            )));
          }
          last_message_seq = Some(seq);
          crate::journal::keep_max(&mut last_seq, seq);
        }
        messages.push(message.clone());
      }
      SessionRecord::Epoch(epoch) => epochs.push(epoch.clone()),
      SessionRecord::Compaction(compaction) => {
        match (compaction.replaces_from, compaction.replaces_through) {
          (None, None) => {}
          (Some(from), Some(through)) if from.0 > 0 && from <= through => {}
          _ => {
            return Err(StoreError::Invalid(format!(
              "{} compaction epoch {} has invalid canonical replacement bounds",
              path.display(),
              compaction.context_epoch
            )));
          }
        }
        if compaction.context_epoch == 0
          || last_compaction_epoch.is_some_and(|previous| compaction.context_epoch <= previous)
        {
          return Err(StoreError::Invalid(format!(
            "{} contains non-monotonic compaction epoch {}",
            path.display(),
            compaction.context_epoch
          )));
        }
        let closes_checkpoint = compaction.level == pi_rs_core::ContextLevel::L3Checkpoint
          && last_checkpoint_epoch == Some(compaction.context_epoch);
        if !closes_checkpoint
          && last_context_epoch.is_some_and(|previous| compaction.context_epoch <= previous)
        {
          return Err(StoreError::Invalid(format!(
            "{} reuses context epoch {} outside its checkpoint boundary",
            path.display(),
            compaction.context_epoch
          )));
        }
        last_compaction_epoch = Some(compaction.context_epoch);
        last_context_epoch = Some(
          last_context_epoch.map_or(compaction.context_epoch, |previous| {
            previous.max(compaction.context_epoch)
          }),
        );
        context_epoch = context_epoch.max(compaction.context_epoch);
        if compaction.level == pi_rs_core::ContextLevel::L3Checkpoint {
          let Some(pre_barrier) = checkpoint_pre_barrier.take() else {
            return Err(StoreError::Invalid(format!(
              "{} checkpoint compaction epoch {} has no preceding barrier",
              path.display(),
              compaction.context_epoch
            )));
          };
          if compaction.summary_present {
            return Err(StoreError::Invalid(format!(
              "{} checkpoint compaction epoch {} cannot claim a summary",
              path.display(),
              compaction.context_epoch
            )));
          }
          let removed = compaction.removed_messages as usize;
          if removed > pre_barrier.len() {
            return Err(StoreError::Invalid(format!(
              "{} checkpoint epoch {} removes {} messages but only {} precede its barrier",
              path.display(),
              compaction.context_epoch,
              removed,
              pre_barrier.len()
            )));
          }
          let retained = pre_barrier.len() - removed;
          // The retained count includes the protected capsule itself. This is
          // the same convention for full and prefix checkpoints and prevents a
          // malformed marker from hiding an arbitrary post-barrier tail.
          let expected_retained = retained
            .checked_add(1)
            .ok_or_else(|| StoreError::Invalid("retained message count is exhausted".into()))?;
          if compaction.retained_messages as usize != expected_retained {
            return Err(StoreError::Invalid(format!(
              "{} checkpoint epoch {} claims {} retained messages, expected {}",
              path.display(),
              compaction.context_epoch,
              compaction.retained_messages,
              expected_retained
            )));
          }
          summarized_messages = summarized_messages
            .checked_add(removed)
            .ok_or_else(|| StoreError::Invalid("summarized message count is exhausted".into()))?;
          messages = pre_barrier.into_iter().skip(removed).collect();
        }
        compactions.push(compaction.clone());
        if compaction.summary_present {
          // Runtime compaction appends its summary after the existing semantic
          // messages, then records this marker. Rebuild the exact live window as
          // [summary, retained tail] without mutating canonical history on disk.
          let summary = messages.pop().ok_or_else(|| {
            StoreError::Invalid(format!(
              "{} compaction epoch {} claims a summary but no summary message precedes it",
              path.display(),
              compaction.context_epoch
            ))
          })?;
          let available = messages.len();
          let retained = compaction.retained_messages as usize;
          if retained > available {
            return Err(StoreError::Invalid(format!(
              "{} compaction epoch {} claims {} retained messages but only {} are available",
              path.display(),
              compaction.context_epoch,
              retained,
              available
            )));
          }
          let expected_removed = available - retained;
          if compaction.removed_messages as usize != expected_removed {
            return Err(StoreError::Invalid(format!(
              "{} compaction epoch {} claims {} removed messages, expected {}",
              path.display(),
              compaction.context_epoch,
              compaction.removed_messages,
              expected_removed
            )));
          }
          let split = available - retained;
          let tail = messages.split_off(split);
          messages.clear();
          messages.push(summary);
          messages.extend(tail);
        }
      }
      SessionRecord::CheckpointBarrier(barrier) => {
        if !seen_checkpoint_ids.insert(barrier.checkpoint_id.clone()) {
          return Err(StoreError::Invalid(format!(
            "{} contains duplicate checkpoint barrier {}",
            path.display(),
            barrier.checkpoint_id
          )));
        }
        // A later barrier supersedes an earlier one: everything before it is
        // already inside the newer capsule's scope. Epoch-bearing barriers are
        // strictly newer than all previous boundaries; zero is retained only
        // for legacy records that predate checkpoint epochs.
        if barrier.context_epoch > 0 {
          if last_context_epoch.is_some_and(|previous| barrier.context_epoch <= previous) {
            return Err(StoreError::Invalid(format!(
              "{} reuses checkpoint context epoch {}",
              path.display(),
              barrier.context_epoch
            )));
          }
          last_context_epoch = Some(barrier.context_epoch);
          last_checkpoint_epoch = Some(barrier.context_epoch);
        } else if last_context_epoch.is_some() {
          return Err(StoreError::Invalid(format!(
            "{} contains a legacy zero checkpoint epoch after an epoch-bearing boundary",
            path.display()
          )));
        }
        if checkpoint_pre_barrier.is_some() {
          return Err(StoreError::Invalid(format!(
            "{} contains a checkpoint barrier without a completion boundary",
            path.display()
          )));
        }
        // Do not discard the pre-barrier projection yet. A prefix checkpoint
        // retains the current-turn suffix, and the following L3 completion is
        // the durable record that tells us where that suffix begins.
        checkpoint_pre_barrier = Some(std::mem::take(&mut messages));
        checkpoint = Some(barrier.capsule.clone());
        checkpoint_seq = last_seq;
        context_epoch = context_epoch.max(barrier.context_epoch);
        messages.clear();
      }
      SessionRecord::Reduction(reduction) => {
        if reduction.removed_messages == 0 {
          return Err(StoreError::Invalid(format!(
            "{} contains a zero-width reduction at event {}",
            path.display(),
            reduction.event_id
          )));
        }
        if !seen_event_ids.insert(reduction.event_id.clone()) {
          return Err(StoreError::Invalid(format!(
            "{} contains duplicate semantic event {}",
            path.display(),
            reduction.event_id
          )));
        }
        if let Some(seq) = reduction.seq {
          if seq.0 == 0 {
            return Err(StoreError::Invalid(format!(
              "{} contains a zero reduction sequence at event {}",
              path.display(),
              reduction.event_id
            )));
          }
          if last_message_seq.is_some_and(|previous| seq <= previous)
            || last_reduction_seq.is_some_and(|previous| seq <= previous)
          {
            return Err(StoreError::Invalid(format!(
              "{} contains a non-monotonic reduction sequence at event {}",
              path.display(),
              reduction.event_id
            )));
          }
          last_reduction_seq = Some(seq);
          crate::journal::keep_max(&mut last_seq, seq);
        }
        let removed = reduction.removed_messages as usize;
        if removed > messages.len() {
          return Err(StoreError::Invalid(format!(
            "{} reduction {} removes {} messages but only {} are retained",
            path.display(),
            reduction.event_id,
            removed,
            messages.len()
          )));
        }
        let expected_retained = messages.len() - removed;
        let declared_retained = reduction.retained_messages as usize;
        let projected_retained = if checkpoint.is_some() {
          declared_retained.checked_sub(1).ok_or_else(|| {
            StoreError::Invalid(format!(
              "{} reduction {} omits its protected checkpoint message",
              path.display(),
              reduction.event_id
            ))
          })?
        } else {
          declared_retained
        };
        if projected_retained != expected_retained {
          return Err(StoreError::Invalid(format!(
            "{} reduction {} claims {} retained messages, expected {}",
            path.display(),
            reduction.event_id,
            reduction.retained_messages,
            expected_retained
          )));
        }
        reductions.push(reduction.clone());
        // Checkpoint capsules are kept separately from `messages`; the runtime
        // prepends the capsule after restore. The semantic log therefore drains
        // only the post-checkpoint tail here.
        messages.drain(..removed);
      }
      SessionRecord::Header(_) => {
        return Err(StoreError::Invalid(format!(
          "{} contains a second session header",
          path.display()
        )));
      }
    }
  }
  if let Some(pre_barrier) = checkpoint_pre_barrier.take() {
    // Session journals from before L3 checkpoint completion was introduced
    // legitimately end at the barrier. The canonical store path separately
    // rejects an incomplete modern lifecycle; this preserves direct session-log
    // compatibility for legacy barriers.
    summarized_messages = summarized_messages
      .checked_add(pre_barrier.len())
      .ok_or_else(|| StoreError::Invalid("summarized message count is exhausted".into()))?;
  }

  Ok(RestoredSession {
    header,
    messages,
    checkpoint,
    checkpoint_seq,
    epochs,
    compactions,
    reductions,
    interrupted_tools: Vec::new(),
    context_epoch,
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
    context::{CAPSULE_SCHEMA_VERSION, ContextCapsule, ContextLevel},
    ids::uuidv7,
    ids::{CheckpointId, EventId, TurnId},
    message::Message,
    session::{
      SessionCheckpointRecord, SessionCompactionRecord, SessionEpochRecord, SessionMessage,
      SessionReductionRecord,
    },
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
      external_context: None,
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
  fn version_one_files_without_new_records_remain_readable() {
    let tmp = TempDir::new("sessionlog-version-one");
    let (layout, id) = session(&tmp);
    let target = path(&layout, &id);
    let old_header = SessionRecord::Header(SessionHeader {
      version: 1,
      ..header(&id)
    });
    std::fs::write(
      &target,
      format!(
        "{}\n{}\n",
        serde_json::to_string(&old_header).unwrap(),
        serde_json::to_string(&message("legacy", 1)).unwrap()
      ),
    )
    .unwrap();
    let report = SessionLog::read(&target).expect("version-one records remain compatible");
    assert_eq!(report.items.len(), 2);
    let restored = restore(&target).expect("legacy session restores without reinterpretation");
    assert_eq!(restored.header.version, 1);
    assert_eq!(restored.messages[0].message.text(), "legacy");
  }

  #[test]
  fn version_one_rejects_the_new_reduction_record_instead_of_migrating_by_guess() {
    let tmp = TempDir::new("sessionlog-reduction-schema");
    let (layout, id) = session(&tmp);
    let target = path(&layout, &id);
    let old_header = SessionRecord::Header(SessionHeader {
      version: 1,
      ..header(&id)
    });
    let reduction = SessionRecord::Reduction(SessionReductionRecord {
      event_id: EventId::new(),
      seq: Some(EventSeq(2)),
      reason: pi_rs_core::context::ReductionReason::RecentTargetExceeded { target_tokens: 128 },
      removed_messages: 1,
      retained_messages: 0,
    });
    std::fs::write(
      &target,
      format!(
        "{}\n{}\n",
        serde_json::to_string(&old_header).unwrap(),
        serde_json::to_string(&reduction).unwrap()
      ),
    )
    .unwrap();
    let error = SessionLog::read(&target).unwrap_err();
    assert!(
      format!("{error}").contains("contains a reduction record"),
      "{error}"
    );
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
        context_epoch: 0,
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
  fn a_prefix_checkpoint_preserves_pre_barrier_retained_messages() {
    let tmp = TempDir::new("sessionlog-prefix-checkpoint");
    let (layout, id) = session(&tmp);
    let target = path(&layout, &id);
    let mut log = SessionLog::create(&target, header(&id)).unwrap();
    log.append(&message("old history", 1)).unwrap();
    log.append(&message("current user", 2)).unwrap();
    log
      .append(&SessionRecord::CheckpointBarrier(SessionCheckpointRecord {
        checkpoint_id: CheckpointId::new(),
        capsule_version: CAPSULE_SCHEMA_VERSION,
        context_epoch: 1,
        capsule_path: "checkpoints/prefix.json".into(),
        capsule: ContextCapsule::new("prefix capsule"),
      }))
      .unwrap();
    log
      .append(&SessionRecord::Compaction(SessionCompactionRecord {
        context_epoch: 1,
        level: ContextLevel::L3Checkpoint,
        removed_messages: 1,
        retained_from: 0,
        retained_messages: 2,
        summary_present: false,
        replaces_from: None,
        replaces_through: None,
      }))
      .unwrap();
    drop(log);

    let restored = restore(&target).unwrap();
    assert_eq!(restored.summarized_messages, 1);
    assert_eq!(restored.messages.len(), 1);
    assert_eq!(restored.messages[0].message.text(), "current user");
    assert_eq!(restored.checkpoint.unwrap().objective, "prefix capsule");
  }

  #[test]
  fn a_second_prefix_checkpoint_counts_only_the_semantic_tail() {
    let tmp = TempDir::new("sessionlog-second-prefix-checkpoint");
    let (layout, id) = session(&tmp);
    let target = path(&layout, &id);
    let mut log = SessionLog::create(&target, header(&id)).unwrap();
    log
      .append(&SessionRecord::CheckpointBarrier(SessionCheckpointRecord {
        checkpoint_id: CheckpointId::new(),
        capsule_version: CAPSULE_SCHEMA_VERSION,
        context_epoch: 1,
        capsule_path: "checkpoints/first.json".into(),
        capsule: ContextCapsule::new("first capsule"),
      }))
      .unwrap();
    log.append(&message("old tail", 1)).unwrap();
    log.append(&message("current user", 2)).unwrap();
    log
      .append(&SessionRecord::CheckpointBarrier(SessionCheckpointRecord {
        checkpoint_id: CheckpointId::new(),
        capsule_version: CAPSULE_SCHEMA_VERSION,
        context_epoch: 2,
        capsule_path: "checkpoints/second.json".into(),
        capsule: ContextCapsule::new("second capsule"),
      }))
      .unwrap();
    log
      .append(&SessionRecord::Compaction(SessionCompactionRecord {
        context_epoch: 2,
        level: ContextLevel::L3Checkpoint,
        removed_messages: 1,
        retained_from: 0,
        retained_messages: 2,
        summary_present: false,
        replaces_from: None,
        replaces_through: None,
      }))
      .unwrap();
    drop(log);

    let restored = restore(&target).unwrap();
    assert_eq!(restored.summarized_messages, 1);
    assert_eq!(restored.messages.len(), 1);
    assert_eq!(restored.messages[0].message.text(), "current user");
    assert_eq!(restored.checkpoint.unwrap().objective, "second capsule");
  }

  #[test]
  fn a_projected_compaction_restores_summary_and_retained_tail() {
    let tmp = TempDir::new("sessionlog-compaction");
    let (layout, id) = session(&tmp);
    let target = path(&layout, &id);
    let mut log = SessionLog::create(&target, header(&id)).unwrap();
    log.append(&message("old", 1)).unwrap();
    log.append(&message("tail", 2)).unwrap();
    log.append(&message("summary", 3)).unwrap();
    log
      .append(&SessionRecord::Compaction(SessionCompactionRecord {
        context_epoch: 1,
        level: ContextLevel::L1Ordinary,
        removed_messages: 1,
        retained_from: 0,
        retained_messages: 1,
        summary_present: true,
        replaces_from: Some(EventSeq(1)),
        replaces_through: Some(EventSeq(2)),
      }))
      .unwrap();
    log.append(&message("new", 4)).unwrap();
    drop(log);

    let restored = restore(&target).unwrap();
    assert_eq!(restored.context_epoch, 1);
    assert_eq!(restored.messages.len(), 3);
    assert_eq!(
      restored
        .messages
        .iter()
        .map(|message| message.message.text())
        .collect::<Vec<_>>(),
      ["summary", "tail", "new"]
    );
  }

  #[test]
  fn a_reduction_preserves_the_checkpoint_capsule() {
    let tmp = TempDir::new("sessionlog-reduction-checkpoint");
    let (layout, id) = session(&tmp);
    let target = path(&layout, &id);
    let mut log = SessionLog::create(&target, header(&id)).unwrap();
    log
      .append(&SessionRecord::CheckpointBarrier(SessionCheckpointRecord {
        checkpoint_id: CheckpointId::new(),
        capsule_version: CAPSULE_SCHEMA_VERSION,
        context_epoch: 1,
        capsule_path: "checkpoints/cp.json".into(),
        capsule: ContextCapsule::new("preserve me"),
      }))
      .unwrap();
    log.append(&message("old tail", 1)).unwrap();
    log.append(&message("new tail", 2)).unwrap();
    log
      .append(&SessionRecord::Reduction(SessionReductionRecord {
        event_id: EventId::new(),
        seq: Some(EventSeq(3)),
        reason: pi_rs_core::ReductionReason::RecentTargetExceeded { target_tokens: 1 },
        removed_messages: 1,
        // The count includes the protected capsule: capsule + new tail.
        retained_messages: 2,
      }))
      .unwrap();
    drop(log);

    let restored = restore(&target).unwrap();
    assert_eq!(restored.messages.len(), 1);
    assert_eq!(restored.messages[0].message.text(), "new tail");
    assert!(
      restored
        .checkpoint
        .unwrap()
        .objective
        .contains("preserve me")
    );
  }

  #[test]
  fn compaction_restore_rejects_missing_summary_and_inexact_boundaries() {
    let cases = [
      (
        "missing-summary",
        vec![SessionRecord::Compaction(SessionCompactionRecord {
          context_epoch: 1,
          level: ContextLevel::L1Ordinary,
          removed_messages: 0,
          retained_from: 0,
          retained_messages: 0,
          summary_present: true,
          replaces_from: None,
          replaces_through: None,
        })],
      ),
      (
        "retained-overflow",
        vec![
          message("only message", 1),
          message("summary", 2),
          SessionRecord::Compaction(SessionCompactionRecord {
            context_epoch: 1,
            level: ContextLevel::L1Ordinary,
            removed_messages: 0,
            retained_from: 0,
            retained_messages: 2,
            summary_present: true,
            replaces_from: None,
            replaces_through: None,
          }),
        ],
      ),
      (
        "removed-mismatch",
        vec![
          message("old", 1),
          message("tail", 2),
          message("summary", 3),
          SessionRecord::Compaction(SessionCompactionRecord {
            context_epoch: 1,
            level: ContextLevel::L1Ordinary,
            removed_messages: 0,
            retained_from: 0,
            retained_messages: 1,
            summary_present: true,
            replaces_from: None,
            replaces_through: None,
          }),
        ],
      ),
    ];
    for (label, records) in cases {
      let tmp = TempDir::new(&format!("sessionlog-compaction-{label}"));
      let (layout, id) = session(&tmp);
      let target = path(&layout, &id);
      let mut log = SessionLog::create(&target, header(&id)).unwrap();
      for record in records {
        log.append(&record).unwrap();
      }
      drop(log);
      assert!(
        matches!(restore(&target), Err(StoreError::Invalid(_))),
        "{label} must fail closed"
      );
    }
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
        context_epoch: 0,
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
