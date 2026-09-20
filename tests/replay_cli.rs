//! End-to-end coverage for deterministic, read-only replay.

use std::{
  fs,
  path::{Path, PathBuf},
  process::Command,
};

use rupi_core::{
  AgentEvent, EventEnvelope, EventMeta, EventSeq, Message, ModelCapabilities, ModelRef,
  ReasoningDelta, ReasoningProvenance, SessionId, SessionMessage, SessionRecord, SessionStarted,
  ToolCallId, ToolUnknown, TraceEntry, TraceId, TurnId, UserMessage,
  session::{SESSION_SCHEMA_VERSION, SessionHeader},
};
use tempfile::TempDir;

fn entry(session: &SessionId, seq: u64, event: AgentEvent) -> TraceEntry {
  let mut meta = EventMeta::new(session.clone(), TraceId::from_string("trace-replay"));
  meta.seq = Some(EventSeq(seq));
  meta.timestamp_ms = 1_700_000_000_000 + seq;
  TraceEntry {
    envelope: EventEnvelope::new(meta, event),
    redactions: 0,
    raw_payload: false,
    raw_ref: None,
    externalized: Vec::new(),
  }
}

fn fixture() -> (TempDir, PathBuf, PathBuf, SessionId) {
  let temp = TempDir::new().unwrap();
  let session = SessionId::from_string("01940000-0000-7000-8000-0000000000f1");
  let trace_path = temp.path().join("session.trace.jsonl");
  let session_path = temp.path().join("session.jsonl");
  let model = ModelRef::new("fake", "agent");
  let call = ToolCallId::from_string("call-write");
  let mut entries = [
    entry(
      &session,
      1,
      AgentEvent::SessionStarted(SessionStarted {
        working_dir: "/repo".into(),
        model: model.clone(),
        capabilities: ModelCapabilities::text_only(32_000),
        resumed: false,
      }),
    ),
    entry(
      &session,
      2,
      AgentEvent::UserMessage(UserMessage {
        text: "fix it".into(),
        attachments: 0,
      }),
    ),
    entry(
      &session,
      3,
      AgentEvent::ReasoningDelta(ReasoningDelta {
        text: "inspect first".into(),
        provenance: ReasoningProvenance::Native,
        chunk_index: 0,
      }),
    ),
    entry(
      &session,
      4,
      AgentEvent::ToolUnknown(ToolUnknown {
        call_id: call.clone(),
        name: "write".into(),
        why: "process ended before completion".into(),
        mutating: true,
      }),
    ),
  ];
  entries[3].raw_payload = true;
  entries[3].raw_ref = Some("blobs/aa/raw-secret".into());
  fs::write(
    &trace_path,
    entries
      .iter()
      .map(|entry| serde_json::to_string(entry).unwrap())
      .collect::<Vec<_>>()
      .join("\n")
      + "\n",
  )
  .unwrap();
  let session_records = [
    SessionRecord::Header(SessionHeader {
      session_id: session.clone(),
      version: SESSION_SCHEMA_VERSION,
      started_at_ms: 1,
      working_dir: "/repo".into(),
      model: model.clone(),
      parent_session: None,
      branched_from_event: None,
      imported_from: None,
    }),
    SessionRecord::Message(SessionMessage {
      turn_id: TurnId::from_string("turn-1"),
      role: rupi_core::Role::User,
      message: Message::user("fix it"),
      epoch: 0,
      model,
      event_id: entries[1].envelope.meta.event_id.clone(),
      seq: Some(EventSeq(2)),
      external_context: None,
    }),
  ];
  fs::write(
    &session_path,
    session_records
      .iter()
      .map(|record| serde_json::to_string(record).unwrap())
      .collect::<Vec<_>>()
      .join("\n")
      + "\n",
  )
  .unwrap();
  (temp, trace_path, session_path, session)
}

fn run(input: &Path, args: &[&str]) -> std::process::Output {
  Command::new(env!("CARGO_BIN_EXE_rupi"))
    .arg("replay")
    .arg(input)
    .args(args)
    .output()
    .unwrap()
}

#[test]
fn replay_filters_and_stops_inclusively_without_generation() {
  let (_temp, trace, _session, _id) = fixture();
  let output = run(&trace, &["--tools", "--sequence"]);
  assert!(
    output.status.success(),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  let text = String::from_utf8(output.stdout).unwrap();
  assert!(text.contains("[4] tool_unknown write"), "{text}");
  assert!(!text.contains("reasoning_delta"), "{text}");

  let output = run(&trace, &["--until=seq:2", "--json"]);
  assert!(
    output.status.success(),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
  assert_eq!(report["report"]["events"].as_array().unwrap().len(), 2);
}

#[test]
fn replay_context_joins_session_projection_and_export_omits_raw_pointer() {
  let (_temp, _trace, session, _id) = fixture();
  let output = run(&session, &["--context-at", "seq:2", "--json"]);
  assert!(
    output.status.success(),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
  assert_eq!(
    report["context"]["working"]["items"][0]["message"]["content"][0]["text"],
    "fix it"
  );

  let export = session.with_file_name("export.jsonl");
  let output = run(&session, &["--export", export.to_str().unwrap(), "--json"]);
  assert!(
    output.status.success(),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  assert!(export.exists());
  let exported = fs::read_to_string(export).unwrap();
  assert!(!exported.contains("raw-secret"), "{exported}");
}
