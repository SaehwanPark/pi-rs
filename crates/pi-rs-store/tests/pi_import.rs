//! Importing a Pi session file into a pi-rs state root.
//!
//! The fixture at `tests/fixtures/pi/branched.jsonl` is committed and hand-written to the
//! shape Pi v3 writes: a model change, reasoning blocks inside the assistant message, a tool
//! call and its result, a compaction boundary, a label, an extension entry, and one sibling
//! branch that was never checked back out. It is committed rather than generated so that a
//! change in the importer is visibly a change against a fixed statement of Pi's format.

use std::{
  fs,
  path::{Path, PathBuf},
};

use pi_rs_core::{AgentEvent, ContentBlock, ModelRef, Role, SessionMessage, ToolExecutionState};
use pi_rs_store::{SessionLog, Store, TraceJournal, WritePolicy, pi_import};

fn fixture() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pi/branched.jsonl")
}

/// Read the committed fixture. A failure here says the fixture moved, not the importer.
fn read_fixture() -> pi_import::PiSession {
  let path = fixture();
  pi_import::read(&path).expect("fixture reads")
}

fn store_in(root: &Path, inline_threshold_bytes: u64) -> Store {
  let policy = WritePolicy {
    inline_threshold_bytes,
    ..WritePolicy::default()
  };
  Store::open(root.join("state"), policy).expect("store opens")
}

#[test]
fn the_report_says_what_was_imported_and_what_was_left_out() {
  let source = read_fixture();
  let plan = pi_import::plan(&source).expect("fixture plans");

  assert_eq!(plan.report.imported.get("message:user"), Some(&2));
  assert_eq!(plan.report.imported.get("message:assistant"), Some(&3));
  assert_eq!(plan.report.imported.get("message:toolResult"), Some(&1));
  assert_eq!(plan.report.imported.get("toolCall"), Some(&1));
  assert_eq!(plan.report.imported.get("model_change"), Some(&1));

  for kind in ["compaction", "label", "custom"] {
    assert!(
      plan.report.skipped.contains_key(kind),
      "fixture's {kind} entry was neither imported nor reported: {:?}",
      plan.report.skipped.keys().collect::<Vec<_>>()
    );
  }
  // e-branch is a sibling of the path Pi's cursor ended on: counted by type, not hidden.
  assert_eq!(plan.report.off_path.get("message"), Some(&1));
  // Pi recorded cache usage and an image; pi-rs's events have no field for either.
  // Kinds an import cannot carry are counted, not quietly dropped. An image is not one of
  // them: pi-rs holds images inline in a session's messages, so an import does the same.
  assert_eq!(plan.report.content.get("image"), None);
  assert_eq!(plan.report.messages, 6);
  assert_eq!(plan.report.content.get("usage:cacheRead"), Some(&1));
}

#[test]
fn the_header_records_the_import_rather_than_pretending() {
  let source = read_fixture();
  let plan = pi_import::plan(&source).expect("fixture plans");

  assert_eq!(plan.header.imported_from.as_deref(), Some("pi"));
  assert_eq!(plan.header.working_dir, "/home/dev/app");
  // The model that produced the last assistant message, which is what an opened session
  // should resume as. Pi's file names it; nothing here guesses.
  assert_eq!(plan.header.model, ModelRef::new("openai", "gpt-5.4-mini"));
  assert_eq!(
    plan.header.session_id.as_str(),
    "pi-a1b2c3d4-0000-7000-8000-000000000001"
  );
  // The first entry that carried a readable timestamp, not the header's own.
  assert_eq!(plan.header.started_at_ms, 1_784_538_010_000);
}

#[test]
fn writing_files_one_session_the_store_can_read_back() {
  let root = StoreTempDir::new("import-write");
  let store = store_in(root.path(), 64);
  let source = read_fixture();
  let plan = pi_import::plan(&source).expect("fixture plans");
  let session_id = pi_import::write(&store, &plan).expect("import writes");
  assert_eq!(session_id, plan.header.session_id);

  let layout = store.layout();
  let journal = TraceJournal::read(&layout.trace_path(&session_id)).expect("journal reads");
  assert_eq!(journal.items.len(), plan.events.len());
  assert_eq!(journal.malformed, 0);
  let sequences: Vec<u64> = journal
    .items
    .iter()
    .map(|entry| entry.envelope.meta.seq.expect("stamped").0)
    .collect();
  let mut sorted = sequences.clone();
  sorted.sort_unstable();
  assert_eq!(
    sequences, sorted,
    "the journal's own ordering must survive the import"
  );

  let header =
    SessionLog::read_header(&layout.session_path(&session_id)).expect("session header reads");
  assert_eq!(header.imported_from.as_deref(), Some("pi"));

  let tool = journal
    .items
    .iter()
    .find(|entry| matches!(entry.envelope.event, AgentEvent::ToolCompleted(_)))
    .expect("one completed tool call");
  let AgentEvent::ToolCompleted(completed) = &tool.envelope.event else {
    unreachable!("filtered above");
  };
  // 121 bytes is over this store's 64-byte threshold, so the trace keeps a blob.
  let blob = completed.blob.clone().expect("output filed as a blob");
  assert!(!completed.reduced, "pi-rs reduced nothing");
  assert_eq!(blob.size, 121);
  let stored = fs::read(layout.blob_path(&session_id, &blob)).expect("blob resolves");
  assert_eq!(stored.len() as u64, blob.size);
  assert!(String::from_utf8(stored).unwrap().contains("src/lib.rs"));
}

#[test]
fn imported_tool_output_follows_the_native_inline_rule() {
  // A tool result's durable copy is its message record, which is what the next request reads,
  // so the trace needs a blob only past the inline threshold. Filing small imported output
  // too would keep the same bytes twice for no reason, and marking it reduced would claim a
  // summary pi-rs never made.
  for (threshold, expect_blob) in [(64u64, true), (4_096, false)] {
    let root = StoreTempDir::new("import-output");
    let store = store_in(root.path(), threshold);
    let source = read_fixture();
    let plan = pi_import::plan(&source).expect("fixture plans");
    let session_id = pi_import::write(&store, &plan).expect("import writes");
    let journal =
      TraceJournal::read(&store.layout().trace_path(&session_id)).expect("journal reads");
    let AgentEvent::ToolCompleted(completed) = &journal
      .items
      .iter()
      .find(|entry| matches!(entry.envelope.event, AgentEvent::ToolCompleted(_)))
      .expect("completed tool call")
      .envelope
      .event
    else {
      unreachable!("filtered above");
    };
    assert!(!completed.reduced, "pi-rs reduced nothing at {threshold}");
    assert_eq!(completed.visible_bytes, 121);
    assert_eq!(
      completed.blob.is_some(),
      expect_blob,
      "threshold {threshold}"
    );
    if let Some(blob) = &completed.blob {
      let stored = fs::read(store.layout().blob_path(&session_id, blob)).expect("blob resolves");
      assert_eq!(stored.len() as u64, blob.size);
      assert!(String::from_utf8(stored).unwrap().contains("src/lib.rs"));
    }
    // Either way the message record holds the output, because that is what a resume reads.
    let restored = store.restore(&session_id).expect("restores");
    let tool = restored
      .messages
      .iter()
      .find(|record| record.role == Role::Tool)
      .expect("the tool result is a message too");
    let ContentBlock::ToolResult(result) = &tool.message.content[0] else {
      panic!("a tool message holds its result block");
    };
    assert!(result.text.contains("src/lib.rs"));
    assert!(!result.reduced);
  }
}

#[test]
fn importing_twice_refuses_rather_than_appending_to_itself() {
  let root = StoreTempDir::new("import-twice");
  let store = store_in(root.path(), 4_096);
  let source = read_fixture();
  let plan = pi_import::plan(&source).expect("fixture plans");
  pi_import::write(&store, &plan).expect("first import writes");
  let error = pi_import::write(&store, &plan).expect_err("second import must refuse");
  assert!(
    matches!(error, pi_rs_store::StoreError::Invalid(_)),
    "{error}"
  );
  // The refused import must not have left a half-written second session behind.
  assert_eq!(
    store
      .layout()
      .list_session_ids()
      .expect("listing")
      .iter()
      .filter(|id| id.as_str().starts_with("pi-"))
      .count(),
    1
  );
}

#[test]
fn a_missing_file_is_named_and_a_directory_is_not_a_session() {
  let error = pi_import::read(Path::new("/nonexistent/pi-session.jsonl"))
    .expect_err("a missing file is an error");
  assert!(matches!(error, pi_import::PiImportError::Unreadable { .. }));
  assert!(error.to_string().contains("/nonexistent/pi-session.jsonl"));

  let root = StoreTempDir::new("import-dir");
  let error = pi_import::read(root.path()).expect_err("a directory is not a session");
  assert!(matches!(error, pi_import::PiImportError::Unreadable { .. }));
}

/// `TempDir` from the store crate's own test helpers: it holds the path, and dropping
/// removes it, so no import test leaves state behind.
struct StoreTempDir {
  _root: tempfile::TempDir,
  path: std::path::PathBuf,
}

impl StoreTempDir {
  fn new(label: &str) -> Self {
    let root = tempfile::TempDir::with_prefix(format!("pi-rs-{label}-")).expect("tempdir");
    let path = root.path().to_path_buf();
    Self { _root: root, path }
  }

  fn path(&self) -> &Path {
    &self.path
  }
}

/// What makes a session resumable is its message log, not its trace: the next request is
/// built from messages. An import therefore has to produce those records, not only events,
/// and in the roles and order the file recorded them.
#[test]
fn an_imported_session_resumes_from_messages_not_only_a_trace() {
  let root = StoreTempDir::new("import-messages");
  let store = store_in(root.path(), 64);
  let source = read_fixture();
  let plan = pi_import::plan(&source).expect("fixture plans");
  let session_id = pi_import::write(&store, &plan).expect("import writes");

  let restored = store
    .restore(&session_id)
    .expect("imported session restores");
  assert_eq!(restored.malformed_records, 0);
  assert_eq!(
    restored.messages.iter().map(describe).collect::<Vec<_>>(),
    vec![
      "user: which files changed?",
      "assistant: Checking the working tree. | call bash",
      "tool: bash ok",
      "assistant: Two tracked paths and three new ones.",
      "user: and this compile error? | image image/png",
      "assistant: Here is the failing line.",
    ],
    "the conversation Pi recorded, in Pi's order, with the blocks a request would send"
  );
  // The compaction boundary stays in the trace only. Importing its summary text as well
  // would put one conversation into the history twice.
  assert_eq!(restored.checkpoint, None);
}

/// Turn identity and message binding are what let `load_session` read a session back without
/// matching ids, and both have to come out of Pi's entry tree rather than be invented.
#[test]
fn message_records_bind_to_the_event_that_introduced_them() {
  let root = StoreTempDir::new("import-bind");
  let store = store_in(root.path(), 64);
  let source = read_fixture();
  let plan = pi_import::plan(&source).expect("fixture plans");
  let session_id = pi_import::write(&store, &plan).expect("import writes");
  let records = store
    .restore(&session_id)
    .expect("imported session restores")
    .messages;

  let journal = TraceJournal::read(&store.layout().trace_path(&session_id)).expect("journal reads");
  let event = |record: &SessionMessage| {
    journal
      .items
      .iter()
      .find(|entry| entry.envelope.meta.seq == record.seq)
      .unwrap_or_else(|| panic!("no journal event at {:?}", record.seq))
      .envelope
      .event
      .clone()
  };
  for record in &records {
    let introduced_by = event(record);
    match (&record.role, &introduced_by) {
      (Role::User, AgentEvent::UserMessage(_))
      | (Role::Assistant, AgentEvent::AssistantDelta(_))
      | (Role::Tool, AgentEvent::ToolCompleted(_)) => {}
      (role, other) => {
        panic!("{role:?} message must bind to the event that introduced it, got {other:?}")
      }
    }
  }

  let first = records
    .iter()
    .find(|record| record.role == Role::Assistant)
    .expect("an assistant message");
  assert_eq!(first.turn_id.as_str(), "turn-e3");
  assert_eq!(first.model.as_key(), "anthropic/claude-opus-4-8");
  assert!(
    !first
      .message
      .content
      .iter()
      .any(|block| matches!(block, ContentBlock::Reasoning { .. })),
    "Pi's stored reasoning belongs to the trace, not to the next request's messages"
  );
  assert_eq!(first.epoch, 0, "a Pi file records no failover");
  let second_turn = records
    .iter()
    .find(|record| record.role == Role::User && record.turn_id.as_str() != "turn-e3")
    .expect("the second user message");
  assert_eq!(second_turn.turn_id.as_str(), "turn-e8");
  let last = records.last().expect("the last message");
  assert_eq!(
    last.model.as_key(),
    "openai/gpt-5.4-mini",
    "the model that produced it, which is what an opened session resumes as"
  );
}

/// A turn id derived from Pi's entry ids is a stable identifier, not a per-run one: importing
/// the same file twice has to produce the same turn structure, or a re-import would look like
/// a different conversation.
#[test]
fn a_reimport_produces_the_same_turns() {
  let turns = |label: &str| -> Vec<(String, Role)> {
    let root = StoreTempDir::new(label);
    let store = store_in(root.path(), 64);
    let source = read_fixture();
    let plan = pi_import::plan(&source).expect("fixture plans");
    let session_id = pi_import::write(&store, &plan).expect("import writes");
    store
      .restore(&session_id)
      .expect("restores")
      .messages
      .iter()
      .map(|record| (record.turn_id.to_string(), record.role))
      .collect()
  };
  let first = turns("reimport-a");
  let second = turns("reimport-b");
  assert_eq!(first, second);
  assert_eq!(first.len(), 6);
  assert_eq!(
    first
      .iter()
      .map(|(turn, _)| turn.as_str())
      .collect::<std::collections::HashSet<_>>()
      .len(),
    2,
    "two user messages open two turns"
  );
}

/// A one-line rendering of what a message record would put in front of a model.
fn describe(record: &SessionMessage) -> String {
  let mut parts: Vec<String> = Vec::new();
  for block in &record.message.content {
    match block {
      ContentBlock::Text { text } => parts.push(text.clone()),
      ContentBlock::ToolCall(call) => parts.push(format!("call {}", call.name)),
      ContentBlock::ToolResult(result) => parts.push(format!(
        "{} {}",
        result.name,
        match result.state {
          ToolExecutionState::Succeeded => "ok",
          ToolExecutionState::Failed => "failed",
          _ => "unknown",
        }
      )),
      ContentBlock::Image { mime, .. } => parts.push(format!("image {mime}")),
      ContentBlock::Reasoning { .. } => parts.push("reasoning".to_string()),
    }
  }
  format!("{}: {}", record.role.as_str(), parts.join(" | "))
}
