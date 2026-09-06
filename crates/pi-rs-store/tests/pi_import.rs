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

use pi_rs_core::{AgentEvent, ModelRef};
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
  assert_eq!(plan.report.content.get("image"), Some(&1));
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
  // The output is a blob because an imported output has no inline home at all.
  let blob = completed.blob.clone().expect("output filed as a blob");
  assert!(!completed.reduced, "pi-rs reduced nothing");
  assert_eq!(blob.size, 121);
  let stored = fs::read(layout.blob_path(&session_id, &blob)).expect("blob resolves");
  assert_eq!(stored.len() as u64, blob.size);
  assert!(String::from_utf8(stored).unwrap().contains("src/lib.rs"));
}

#[test]
fn imported_tool_output_is_filed_whatever_its_size() {
  // pi-rs keeps a tool result in the session's message log, which an import does not write,
  // and `ToolCompleted` has no inline field to carry one. So the output's durable home is a
  // blob at either threshold: a store that keeps small payloads inline must not be able to
  // lose an imported output, and pi-rs must not claim it reduced what it never showed.
  for threshold in [64u64, 4_096] {
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
    let blob = completed.blob.clone().expect("output filed at {threshold}");
    assert!(!completed.reduced);
    assert_eq!(completed.visible_bytes, 121);
    let stored = fs::read(store.layout().blob_path(&session_id, &blob)).expect("blob resolves");
    assert_eq!(stored.len() as u64, blob.size);
    assert!(String::from_utf8(stored).unwrap().contains("src/lib.rs"));
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
