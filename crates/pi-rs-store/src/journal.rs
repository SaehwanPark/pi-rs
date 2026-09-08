//! High-resolution trace journal.
//!
//! The journal is the canonical execution record: every important event, in
//! `seq` order, redacted at this boundary rather than at call sites, with raw
//! provider payloads only when capture is explicitly enabled.
//!
//! Two decisions shape the implementation:
//!
//! - **Redaction happens here.** Writing a line is the only moment where
//!   durable text is assembled, so sanitizing anywhere else would be optional
//!   and therefore forgettable. The count is recorded in the line, which makes
//!   "was this sanitized?" answerable from the file itself.
//! - **Sequence numbers are assigned by the log.** Producers may pass an
//!   envelope with no sequence; the journal is the authority, and reopening a
//!   session recovers the last one from the journal tail rather than by
//!   hydrating it.

use std::{fs, path::Path};

use pi_rs_core::{
  event::{AgentEvent, EventEnvelope},
  ids::{EventSeq, TraceId},
  redact::RedactionPolicy,
  trace::{RawPayloadCapture, TraceEntry},
};
use serde_json::{Value, json};

use crate::{
  StoreError,
  blob::BlobStore,
  jsonl::{LineWriter, ReadReport, read_jsonl, read_jsonl_tail},
  payload,
};

/// Bytes read from the end of a journal to recover the last sequence number.
const SEQUENCE_RECOVERY_WINDOW: u64 = 256 * 1024;

/// Writer for one session's trace journal.
#[derive(Debug)]
pub struct TraceJournal {
  writer: LineWriter,
  policy: RedactionPolicy,
  raw_capture: RawPayloadCapture,
  last_seq: Option<EventSeq>,
  malformed: usize,
}

impl TraceJournal {
  /// Open, or create, the journal for one session and recover its position.
  pub fn open(
    path: &Path,
    policy: RedactionPolicy,
    raw_capture: RawPayloadCapture,
  ) -> Result<Self, StoreError> {
    let mut last_seq: Option<EventSeq> = None;
    let mut malformed = 0usize;
    if path.exists() {
      let tail: ReadReport<TraceEntry> = read_jsonl_tail(path, SEQUENCE_RECOVERY_WINDOW)?;
      malformed = tail.malformed;
      for entry in &tail.items {
        if let Some(seq) = entry.envelope.meta.seq {
          keep_max(&mut last_seq, seq);
        }
      }
    }
    Ok(Self {
      writer: LineWriter::create(path)?,
      policy,
      raw_capture,
      last_seq,
      malformed,
    })
  }

  pub fn path(&self) -> &Path {
    self.writer.path()
  }

  /// Last sequence number that reached this journal, if any.
  pub fn last_seq(&self) -> Option<EventSeq> {
    self.last_seq
  }

  /// Unusable lines seen while recovering the position.
  pub fn malformed_lines(&self) -> usize {
    self.malformed
  }

  /// Append one event, assigning the next sequence number.
  ///
  /// Returns the sequence actually used, which callers must carry into session
  /// records so a message can be traced back into the journal.
  ///
  /// This path does not bound the line: it is for callers that own no blob store
  /// and therefore have nowhere to put bytes they removed. Writes made through
  /// [`crate::Session`] are bounded.
  pub fn append(&mut self, envelope: &EventEnvelope) -> Result<EventSeq, StoreError> {
    self.append_bounded(envelope, None, None, u64::MAX)
  }

  /// Append one event with an optional raw-payload recovery pointer.
  ///
  /// The pointer is recorded only when raw capture is enabled; a caller that
  /// stored a payload anyway cannot make it reachable through a quiet line,
  /// which keeps "raw capture is opt-in" true at the write boundary.
  pub fn append_with(
    &mut self,
    envelope: &EventEnvelope,
    raw_ref: Option<&str>,
  ) -> Result<EventSeq, StoreError> {
    self.append_bounded(envelope, raw_ref, None, u64::MAX)
  }

  /// Append one event with the line held to `budget` bytes.
  ///
  /// When the line would be longer than the budget, whole fields go to `blobs`
  /// and the line keeps a bounded preview that names the reference and the
  /// original size. The redaction policy runs first, so the bytes that leave the
  /// line are already the sanitized ones; bounding before redaction would move
  /// secrets out of reach of the only sanitizer in this crate.
  ///
  /// Without a `blobs` store there is nowhere for removed bytes to go, and a
  /// line is never shortened into a loss: the budget is then simply unmet.
  pub fn append_bounded(
    &mut self,
    envelope: &EventEnvelope,
    raw_ref: Option<&str>,
    blobs: Option<&BlobStore>,
    budget: u64,
  ) -> Result<EventSeq, StoreError> {
    let seq = EventSeq(self.last_seq.map(|seq| seq.0 + 1).unwrap_or(1));
    let raw_attached = raw_ref.filter(|_| self.raw_capture.is_enabled());
    let mut entry = TraceEntry {
      envelope: envelope.clone(),
      redactions: 0,
      raw_payload: raw_attached.is_some(),
      raw_ref: raw_attached.map(str::to_string),
      externalized: Vec::new(),
    };
    entry.envelope.meta.seq = Some(seq);

    let mut line = serde_json::to_value(&entry)?;
    let redactions = self.policy.apply_json(&mut line);
    if redactions > 0 {
      set_field(&mut line, "redactions", json!(redactions));
    }
    let mut text = compact_line(&line)?;
    // An ordinary line is already inside its budget, so bounding costs one length
    // comparison and never runs. Only an oversized line pays for the search.
    if text.len() as u64 > budget {
      if let Some(blobs) = blobs {
        let externalized = payload::bound(&mut line, blobs, budget, |value| {
          Ok(compact_line(value)?.len() as u64)
        })?;
        if !externalized.is_empty() {
          set_field(
            &mut line,
            "externalized",
            serde_json::to_value(&externalized)?,
          );
          text = compact_line(&line)?;
        }
      }
    }
    // Streaming deltas are the high-frequency case; everything that changes
    // state is written through so that a crash cannot lose a transition.
    let durable = requires_durable_write(&entry.envelope.event);
    self.writer.write_line(&text, durable)?;
    self.last_seq = Some(seq);
    Ok(seq)
  }

  /// Reserve a sequence number without writing, for callers that must attach it
  /// to a session record and a journal line for the same fact.
  pub fn next_seq(&self) -> EventSeq {
    EventSeq(self.last_seq.map(|seq| seq.0 + 1).unwrap_or(1))
  }

  /// Commit buffered lines.
  pub fn flush(&mut self) -> Result<(), StoreError> {
    self.writer.flush()
  }

  /// Read the whole journal. Prefer [`TraceJournal::read_tail`] for resumption.
  pub fn read(path: &Path) -> Result<ReadReport<TraceEntry>, StoreError> {
    read_jsonl(path)
  }

  /// Read only the bounded tail window of a journal.
  ///
  /// This is how a read-only path recovers the current position: resume needs
  /// the last sequence number, not the whole history, and a session that has
  /// been open for a week should not cost a full file read to answer "where did
  /// we stop?".
  pub fn read_tail(path: &Path) -> Result<ReadReport<TraceEntry>, StoreError> {
    if !path.exists() {
      return Ok(ReadReport {
        items: Vec::new(),
        malformed: 0,
        first_malformed_line: None,
      });
    }
    read_jsonl_tail(path, SEQUENCE_RECOVERY_WINDOW)
  }

  /// Read events after one sequence number, in order.
  ///
  /// This is the resumption primitive: `latest checkpoint + events after it`.
  pub fn read_after(path: &Path, seq: EventSeq) -> Result<ReadReport<TraceEntry>, StoreError> {
    let mut report = Self::read(path)?;
    report.items.retain(|entry| {
      entry
        .envelope
        .meta
        .seq
        .map(|candidate| candidate.0 > seq.0)
        .unwrap_or(false)
    });
    report.items.sort_by_key(|entry| {
      entry
        .envelope
        .meta
        .seq
        .map(|candidate| candidate.0)
        .unwrap_or(u64::MAX)
    });
    Ok(report)
  }

  /// Trace identifier shared by a session's events, when the journal has any.
  pub fn trace_id(path: &Path) -> Result<Option<TraceId>, StoreError> {
    Ok(
      Self::read(path)?
        .items
        .first()
        .map(|entry| entry.envelope.meta.trace_id.clone()),
    )
  }

  /// Bytes on disk for this journal.
  pub fn size(path: &Path) -> Result<u64, StoreError> {
    match fs::metadata(path) {
      Ok(metadata) => Ok(metadata.len()),
      Err(error) if StoreError::is_missing(&error) => Ok(0),
      Err(error) => Err(StoreError::Io(error)),
    }
  }
}

/// Whether an event must reach disk before `append` returns.
///
/// Rationale for each class: reasoning and text deltas are reconstructible
/// prose whose loss is bounded and visible; anything that records a decision,
/// a tool side effect, a model epoch, or a durability barrier is not, and
/// losing one of those silently rewrites history.
pub fn requires_durable_write(event: &AgentEvent) -> bool {
  !matches!(
    event,
    AgentEvent::ReasoningDelta(_) | AgentEvent::AssistantDelta(_)
  )
}

/// Keep the larger sequence number.
///
/// Written out instead of using `max` because `EventSeq` ordering is total and
/// the intent ("the log's position never moves backwards") is the point.
pub(crate) fn keep_max(current: &mut Option<EventSeq>, candidate: EventSeq) {
  match current {
    Some(existing) if existing.0 >= candidate.0 => {}
    _ => *current = Some(candidate),
  }
}

fn set_field(line: &mut Value, key: &str, value: Value) {
  if let Some(object) = line.as_object_mut() {
    object.insert(key.to_string(), value);
  }
}

fn compact_line(line: &Value) -> Result<String, StoreError> {
  serde_json::to_string(line).map_err(StoreError::from)
}

#[cfg(test)]
mod tests {
  use pi_rs_core::{
    event::{
      AssistantDelta, CheckpointCreated, Diagnostic, DiagnosticLevel, EventMeta, ModelFailover,
      ModelRequestCompleted, ReasoningDelta, SessionStarted, ToolCompleted, ToolRequested,
      TurnCompleted,
    },
    ids::{CheckpointId, SessionId, ToolCallId, TurnId},
    tool::ToolExecutionState,
  };

  use pi_rs_core::BlobRef;

  use crate::StateLayout;
  use crate::tmp::TempDir;

  use super::*;

  fn meta() -> EventMeta {
    EventMeta::new(SessionId::new(), TraceId::new())
  }

  fn envelope(event: AgentEvent) -> EventEnvelope {
    EventEnvelope::new(meta(), event)
  }

  fn diagnostic(message: &str) -> AgentEvent {
    AgentEvent::Diagnostic(Diagnostic {
      level: DiagnosticLevel::Info,
      message: message.to_string(),
    })
  }

  fn journal(tmp: &TempDir, policy: RedactionPolicy) -> TraceJournal {
    TraceJournal::open(
      &tmp.child("trace.jsonl"),
      policy,
      RawPayloadCapture::Disabled,
    )
    .unwrap()
  }

  #[test]
  fn sequence_numbers_are_assigned_by_the_journal() {
    let tmp = TempDir::new("journal-seq");
    let mut journal = journal(&tmp, RedactionPolicy::default());
    assert_eq!(journal.last_seq(), None);
    assert_eq!(journal.next_seq(), EventSeq(1));
    for _ in 0..3 {
      journal.append(&envelope(diagnostic("x"))).unwrap();
    }
    assert_eq!(journal.last_seq(), Some(EventSeq(3)));
    assert_eq!(journal.next_seq(), EventSeq(4));

    let entries = TraceJournal::read(journal.path()).unwrap();
    let seqs: Vec<u64> = entries
      .items
      .iter()
      .map(|entry| entry.envelope.meta.seq.map(|seq| seq.0).unwrap_or(0))
      .collect();
    assert_eq!(seqs, vec![1, 2, 3], "the log is the ordering authority");
    assert_eq!(entries.malformed, 0);
  }

  #[test]
  fn reopening_recovers_the_position_without_hydrating() {
    let tmp = TempDir::new("journal-reopen");
    let path = tmp.child("trace.jsonl");
    {
      let mut journal = TraceJournal::open(
        &path,
        RedactionPolicy::default(),
        RawPayloadCapture::Disabled,
      )
      .unwrap();
      for chunk in 0..2_000 {
        journal
          .append(&envelope(AgentEvent::AssistantDelta(AssistantDelta {
            text: " ".repeat(200),
            chunk_index: chunk,
          })))
          .unwrap();
      }
      journal.flush().unwrap();
    }
    let reopened = TraceJournal::open(
      &path,
      RedactionPolicy::default(),
      RawPayloadCapture::Disabled,
    )
    .unwrap();
    assert_eq!(reopened.last_seq(), Some(EventSeq(2_000)));
    assert_eq!(reopened.next_seq(), EventSeq(2_001));
    assert_eq!(reopened.malformed_lines(), 0);
    assert!(
      TraceJournal::size(&path).unwrap() > SEQUENCE_RECOVERY_WINDOW,
      "the test must actually exceed the recovery window"
    );
  }

  #[test]
  fn durable_writes_survive_without_a_final_flush() {
    let tmp = TempDir::new("journal-durable");
    let path = tmp.child("trace.jsonl");
    {
      // Deliberately no flush: the tool transition must already be on disk.
      let mut journal = TraceJournal::open(
        &path,
        RedactionPolicy::default(),
        RawPayloadCapture::Disabled,
      )
      .unwrap();
      journal
        .append(&envelope(AgentEvent::AssistantDelta(AssistantDelta {
          text: "buffered delta".into(),
          chunk_index: 0,
        })))
        .unwrap();
      assert_eq!(
        TraceJournal::read(&path).unwrap().items.len(),
        0,
        "a delta alone must not touch the disk"
      );
      journal
        .append(&envelope(AgentEvent::ToolRequested(ToolRequested {
          call_id: ToolCallId::new(),
          name: "write".into(),
          arguments: json!({ "path": "src/main.rs" }),
          read_only: false,
        })))
        .unwrap();
      // The transition forces its own way through, and the buffered delta leaves
      // with it. Order is what matters: the delta was produced first, so it must
      // be written first, or the journal would state a false sequence.
      let on_disk = TraceJournal::read(&path).unwrap();
      assert_eq!(on_disk.items.len(), 2, "the transition is on disk unsynced");
      assert!(matches!(
        on_disk.items[0].envelope.event,
        AgentEvent::AssistantDelta(_)
      ));
      assert!(matches!(
        on_disk.items[1].envelope.event,
        AgentEvent::ToolRequested(_)
      ));
      assert_eq!(on_disk.items[1].envelope.meta.seq, Some(EventSeq(2)));
    }
    let entries = TraceJournal::read(&path).unwrap();
    assert_eq!(entries.items.len(), 2, "nothing was lost at drop");
    assert!(matches!(
      entries.items[1].envelope.event,
      AgentEvent::ToolRequested(_)
    ));
  }

  #[test]
  fn classification_covers_every_state_changing_event() {
    assert!(!requires_durable_write(&AgentEvent::ReasoningDelta(
      ReasoningDelta {
        text: "thinking".into(),
        provenance: pi_rs_core::provenance::ReasoningProvenance::Native,
        chunk_index: 0,
      }
    )));
    assert!(!requires_durable_write(&AgentEvent::AssistantDelta(
      AssistantDelta {
        text: "token".into(),
        chunk_index: 0,
      }
    )));
    let transitions = vec![
      AgentEvent::SessionStarted(SessionStarted {
        working_dir: ".".into(),
        model: pi_rs_core::capability::ModelRef::new("local", "m"),
        capabilities: pi_rs_core::capability::ModelCapabilities::text_only(8_000),
        resumed: false,
      }),
      AgentEvent::ModelRequestCompleted(ModelRequestCompleted {
        reasoning_provenance: Some(pi_rs_core::provenance::ReasoningProvenance::Native),
        epoch: 0,
        model: pi_rs_core::capability::ModelRef::new("local", "m"),
        finish_reason: Some("stop".into()),
        input_tokens: Some(10),
        output_tokens: Some(4),
        duration_ms: 12,
        tool_calls: 0,
      }),
      AgentEvent::ModelFailover(ModelFailover {
        from: pi_rs_core::capability::ModelRef::new("a", "a"),
        to: pi_rs_core::capability::ModelRef::new("b", "b"),
        kind: pi_rs_core::failure::ModelFailureKind::ProviderUnavailable,
        gaps: vec![],
        compacted: true,
      }),
      AgentEvent::ToolCompleted(ToolCompleted {
        call_id: ToolCallId::new(),
        name: "write".into(),
        state: ToolExecutionState::Succeeded,
        duration_ms: 1,
        status: None,
        reduced: false,
        blob: None,
        visible_bytes: 2,
      }),
      AgentEvent::CheckpointCreated(CheckpointCreated {
        checkpoint_id: CheckpointId::new(),
        capsule_version: pi_rs_core::context::CAPSULE_SCHEMA_VERSION,
        summarized_events: 12,
        path: "checkpoints/x.json".into(),
      }),
      AgentEvent::TurnCompleted(TurnCompleted {
        status: pi_rs_core::event::TurnStatus::Completed,
        duration_ms: 5,
      }),
    ];
    for event in transitions {
      assert!(requires_durable_write(&event), "{event:?} must be durable");
    }
  }

  #[test]
  fn redaction_happens_at_the_write_boundary() {
    let tmp = TempDir::new("journal-redact");
    let policy = RedactionPolicy {
      literals: vec!["project-token-value".to_string()],
      ..RedactionPolicy::default()
    };
    let mut journal = journal(&tmp, policy);
    journal
      .append(&envelope(diagnostic("loaded project-token-value from env")))
      .unwrap();
    journal.flush().unwrap();

    let raw = fs::read_to_string(journal.path()).unwrap();
    assert!(!raw.contains("project-token-value"), "leaked: {raw}");
    assert!(raw.contains("[redacted:field]"), "{raw}");
    assert!(raw.contains("\"redactions\":1"), "{raw}");

    let entries = TraceJournal::read(journal.path()).unwrap();
    assert_eq!(entries.items[0].redactions, 1);
    let AgentEvent::Diagnostic(inner) = &entries.items[0].envelope.event else {
      panic!("expected diagnostic");
    };
    assert!(inner.message.contains("[redacted:field]"));
  }

  #[test]
  fn raw_payload_pointers_are_recorded_only_when_enabled() {
    let tmp = TempDir::new("journal-raw");
    let path = tmp.child("trace.jsonl");
    let mut quiet = TraceJournal::open(
      &path,
      RedactionPolicy::default(),
      RawPayloadCapture::Disabled,
    )
    .unwrap();
    quiet
      .append_with(
        &envelope(diagnostic("response")),
        Some("blobs/ab/abcd:abcdef012345"),
      )
      .unwrap();
    quiet.flush().unwrap();
    let line = fs::read_to_string(&path).unwrap();
    assert!(
      !line.contains("blobs/"),
      "quiet mode must not reference raw bytes: {line}"
    );
    let entries = TraceJournal::read(&path).unwrap();
    assert!(!entries.items[0].raw_payload);
    assert_eq!(entries.items[0].raw_ref, None);

    let mut capturing = TraceJournal::open(
      &path,
      RedactionPolicy::default(),
      RawPayloadCapture::Enabled,
    )
    .unwrap();
    capturing
      .append_with(
        &envelope(diagnostic("response")),
        Some("blobs/ab/abcd:abcdef012345"),
      )
      .unwrap();
    capturing.flush().unwrap();
    let entries = TraceJournal::read(&path).unwrap();
    let last = entries.items.last().unwrap();
    assert!(last.raw_payload);
    assert_eq!(last.raw_ref.as_deref(), Some("blobs/ab/abcd:abcdef012345"));
  }

  #[test]
  fn read_after_returns_only_later_events_in_order() {
    let tmp = TempDir::new("journal-after");
    let mut journal = journal(&tmp, RedactionPolicy::default());
    for index in 1..=6u64 {
      journal
        .append(&envelope(AgentEvent::TurnCompleted(TurnCompleted {
          status: pi_rs_core::event::TurnStatus::Completed,
          duration_ms: index,
        })))
        .unwrap();
    }
    journal.flush().unwrap();
    let after = TraceJournal::read_after(journal.path(), EventSeq(4)).unwrap();
    let durations: Vec<u64> = after
      .items
      .iter()
      .map(|entry| match &entry.envelope.event {
        AgentEvent::TurnCompleted(turn) => turn.duration_ms,
        other => unreachable!("{other:?}"),
      })
      .collect();
    assert_eq!(durations, vec![5, 6]);
    assert_eq!(
      TraceJournal::read_after(journal.path(), EventSeq(0))
        .unwrap()
        .items
        .len(),
      6
    );
  }

  #[test]
  fn corrupt_lines_are_skipped_and_counted() {
    let tmp = TempDir::new("journal-corrupt");
    let path = tmp.child("trace.jsonl");
    {
      let mut journal = TraceJournal::open(
        &path,
        RedactionPolicy::default(),
        RawPayloadCapture::Disabled,
      )
      .unwrap();
      journal.append(&envelope(diagnostic("one"))).unwrap();
      journal.append(&envelope(diagnostic("two"))).unwrap();
      journal.flush().unwrap();
    }
    fs::OpenOptions::new()
      .append(true)
      .open(&path)
      .and_then(|mut file| {
        use std::io::Write;
        file.write_all(b"trunc\n")
      })
      .unwrap();
    let entries = TraceJournal::read(&path).unwrap();
    assert_eq!(entries.items.len(), 2);
    assert_eq!(entries.malformed, 1);
    let reopened = TraceJournal::open(
      &path,
      RedactionPolicy::default(),
      RawPayloadCapture::Disabled,
    )
    .unwrap();
    assert_eq!(reopened.last_seq(), Some(EventSeq(2)));
    assert_eq!(reopened.malformed_lines(), 1);
  }

  #[test]
  fn trace_id_is_stable_across_a_session() {
    let tmp = TempDir::new("journal-trace");
    let mut journal = journal(&tmp, RedactionPolicy::default());
    let first = envelope(diagnostic("one"));
    let trace_id = first.meta.trace_id.clone();
    journal.append(&first).unwrap();
    journal.flush().unwrap();
    assert_eq!(
      TraceJournal::trace_id(journal.path()).unwrap(),
      Some(trace_id)
    );
    assert_eq!(
      TraceJournal::trace_id(&tmp.child("absent.jsonl")).ok(),
      None,
      "a missing journal is missing, not empty"
    );
  }

  #[test]
  fn turn_identity_travels_into_the_journal() {
    let tmp = TempDir::new("journal-turn");
    let turn = TurnId::new();
    let mut journal = journal(&tmp, RedactionPolicy::default());
    journal
      .append(&envelope(diagnostic("x")).with_meta_turn(turn.clone()))
      .unwrap();
    journal.flush().unwrap();
    let entries = TraceJournal::read(journal.path()).unwrap();
    assert_eq!(entries.items[0].envelope.meta.turn_id.as_ref(), Some(&turn));
  }

  /// Test-only helper so that turn attachment is exercised without duplicating
  /// metadata construction.
  trait WithMetaTurn {
    fn with_meta_turn(self, turn: TurnId) -> EventEnvelope;
  }

  impl WithMetaTurn for EventEnvelope {
    fn with_meta_turn(mut self, turn: TurnId) -> Self {
      self.meta.turn_id = Some(turn);
      self
    }
  }

  /// A session and the blob store that its lines may point into.
  fn session_blobs(tmp: &TempDir) -> (SessionId, BlobStore) {
    let session = SessionId::new();
    let blobs = BlobStore::for_session(&StateLayout::new(tmp.path()), &session).unwrap();
    (session, blobs)
  }

  /// The largest field the model can hand us is a tool argument: a `write`
  /// request carries the file contents it was asked to produce.
  fn huge_write_request(session: &SessionId, contents: &str) -> EventEnvelope {
    EventEnvelope::new(
      EventMeta::new(session.clone(), TraceId::new()),
      AgentEvent::ToolRequested(ToolRequested {
        call_id: ToolCallId::new(),
        name: "write".into(),
        arguments: json!({"path": "generated/data.txt", "contents": contents}),
        read_only: false,
      }),
    )
  }

  /// Reopen the same journal file, which is what a later process does.
  fn reopen(tmp: &TempDir) -> TraceJournal {
    journal(tmp, RedactionPolicy::default())
  }

  fn lines(path: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(path)
      .unwrap()
      .lines()
      .map(str::to_string)
      .collect()
  }

  #[test]
  fn a_bounded_append_keeps_the_written_line_inside_its_budget() {
    let tmp = TempDir::new("journal-bounded");
    let mut journal = journal(&tmp, RedactionPolicy::default());
    let (session, blobs) = session_blobs(&tmp);
    let contents = "x".repeat(60 * 1024);
    let seq = journal
      .append_bounded(
        &huge_write_request(&session, &contents),
        None,
        Some(&blobs),
        2 * 1024,
      )
      .unwrap();
    assert_eq!(seq, EventSeq(1));

    let written = lines(tmp.path().join("trace.jsonl").as_path());
    assert_eq!(written.len(), 1);
    assert!(
      written[0].len() <= 2 * 1024,
      "the unit a reader pays for is the line: {} bytes",
      written[0].len()
    );
    let entry: TraceEntry = serde_json::from_str(&written[0]).unwrap();
    assert_eq!(entry.externalized.len(), 1);
    assert_eq!(entry.externalized[0].field, "arguments/contents");
    assert_eq!(entry.externalized[0].bytes, contents.len() as u64);

    // The line still says which tool was asked and for what path; only the bulk
    // argument moved.
    assert!(written[0].contains("generated/data.txt"));
    assert!(
      !written[0].contains(&"x".repeat(1024)),
      "the bulk argument is not inline"
    );

    // And the bytes are where the record claims they are.
    let blob = BlobRef::for_bytes(contents.as_bytes(), None);
    assert_eq!(entry.externalized[0].reference, blob.relative_path());
    assert_eq!(blobs.get(&blob).unwrap(), contents.as_bytes());
  }

  #[test]
  fn bounding_spills_the_bytes_the_redaction_policy_already_ran_over() {
    // Order matters absolutely: bounding first would move unredacted bytes into
    // the blob store, where the journal's only sanitizer never touches them.
    let tmp = TempDir::new("journal-bounded-redaction");
    let policy = RedactionPolicy {
      literals: vec!["hunter2passphrase".into()],
      scan_environment: false,
      ..RedactionPolicy::default()
    };
    let mut journal = journal(&tmp, policy);
    let (session, blobs) = session_blobs(&tmp);
    let contents = format!("token=hunter2passphrase {}", "p".repeat(40 * 1024));
    journal
      .append_bounded(
        &huge_write_request(&session, &contents),
        None,
        Some(&blobs),
        2 * 1024,
      )
      .unwrap();

    let written = lines(tmp.path().join("trace.jsonl").as_path());
    assert!(!written[0].contains("hunter2passphrase"), "not in the line");
    let entry: TraceEntry = serde_json::from_str(&written[0]).unwrap();
    assert!(
      entry.redactions > 0,
      "the line records that it was sanitized"
    );

    let stored = blobs
      .get_relative(&entry.externalized[0].reference)
      .unwrap();
    let stored = String::from_utf8(stored).unwrap();
    assert!(!stored.contains("hunter2passphrase"), "not in the blob");
    // The marker names the class it came from, e.g. `[redacted:literal]`.
    assert!(stored.contains("[redacted:"), "the blob is sanitized");
  }

  #[test]
  fn an_ordinary_line_never_touches_the_blob_store() {
    // Bounding is a cost on the write path, so it must be conditional: a line
    // that fits may not pay for a store it does not need.
    let tmp = TempDir::new("journal-bounded-fast-path");
    let mut journal = journal(&tmp, RedactionPolicy::default());
    let (session, blobs) = session_blobs(&tmp);
    let mut envelope = huge_write_request(&session, "short");
    envelope.meta.seq = None;
    journal
      .append_bounded(&envelope, None, Some(&blobs), 4 * 1024)
      .unwrap();
    assert_eq!(blobs.bytes().unwrap(), 0, "no store traffic");
    let written = lines(tmp.path().join("trace.jsonl").as_path());
    assert!(written[0].contains("short"));
    let entry: TraceEntry = serde_json::from_str(&written[0]).unwrap();
    assert!(entry.externalized.is_empty(), "and no record of it");
    assert!(
      !written[0].contains("externalized"),
      "the field is not even named"
    );
  }

  #[test]
  fn a_journal_without_a_blob_store_shortens_nothing() {
    // Bounding needs somewhere to put bytes. Without one, the honest outcome is
    // a long line, not a quiet loss.
    let tmp = TempDir::new("journal-bounded-no-where-to-go");
    let mut journal = journal(&tmp, RedactionPolicy::default());
    let (session, _blobs) = session_blobs(&tmp);
    let contents = "y".repeat(20 * 1024);
    journal
      .append_bounded(&huge_write_request(&session, &contents), None, None, 64)
      .unwrap();
    let written = lines(tmp.path().join("trace.jsonl").as_path());
    assert!(written[0].contains(&"y".repeat(1024)), "still inline");
    let entry: TraceEntry = serde_json::from_str(&written[0]).unwrap();
    assert!(entry.externalized.is_empty());
  }

  #[test]
  fn a_bounded_line_survives_a_reopen() {
    let tmp = TempDir::new("journal-bounded-reopen");
    let mut journal = journal(&tmp, RedactionPolicy::default());
    let (session, blobs) = session_blobs(&tmp);
    journal
      .append_bounded(
        &huge_write_request(&session, &"z".repeat(50 * 1024)),
        None,
        Some(&blobs),
        1024,
      )
      .unwrap();
    let reopened = reopen(&tmp);
    assert_eq!(reopened.last_seq(), Some(EventSeq(1)));
    assert_eq!(
      reopened.malformed_lines(),
      0,
      "a bounded line is still a line"
    );
    let entries = TraceJournal::read(tmp.path().join("trace.jsonl").as_path()).unwrap();
    assert_eq!(entries.items.len(), 1);
    assert_eq!(entries.items[0].externalized.len(), 1);
    assert_eq!(entries.malformed, 0);
  }
}
