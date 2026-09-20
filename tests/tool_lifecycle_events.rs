//! Prove that a tool action leaves durable lifecycle records in the trace journal.
//!
//! Four invariants, checked end-to-end: the real binary drives a turn that performs
//! more than one tool action against the deterministic fake provider, and the trace
//! is then read back out of the store. What the journal must show is that
//!
//! 1. one durable call id ties every record of one tool action together,
//! 2. each tool action leaves exactly one terminal record, ordered after the record
//!    that started it,
//! 3. a failing action is closed by a failure record, not a bare success, and
//! 4. an action whose completion the runtime could not observe stays unknown: it is
//!    never recorded as either success or failure.

use std::{
  fs,
  path::{Path, PathBuf},
  process::Command,
};

mod fake_provider;

use rupi_core::{
  AgentEvent, ModelCapabilities, ModelEndpoint, ModelRef, ReasoningExposure, RuntimeConfig,
};
use rupi_store::{StateLayout, TraceJournal};
use tempfile::TempDir;

use fake_provider::{FakeServer, text_response, tool_call};

/// Provider-supplied call ids. That these exact ids reach the journal is part of the
/// durable-id claim: the id the model emitted is the id stored on disk.
const WRITE_CALL: &str = "call_write";
const EXEC_CALL: &str = "call_exec";

/// The action scripted to fail, and the action whose completion cannot be observed.
const FAILING_CALL: &str = "call_failing";
const UNOBSERVED_CALL: &str = "call_unobserved";

/// Every record of the `write` action carries one id, and that id is the one the
/// provider emitted.
#[test]
fn one_durable_call_id_ties_every_record_of_a_tool_action() {
  let records = tool_turn_records();

  let mut action_ids: Vec<&str> = Vec::new();
  for (name, provider_id) in [("write", WRITE_CALL), ("exec", EXEC_CALL)] {
    let action: Vec<&Record> = records
      .iter()
      .filter(|record| tool_name(&record.event) == Some(name))
      .collect();
    assert!(
      action.len() > 1,
      "{name} left {action:?}, which is not a lifecycle a shared id could tie together",
    );

    let ids: Vec<&str> = action
      .iter()
      .map(|record| call_id(&record.event).expect("lifecycle record carries a call id"))
      .collect();
    assert!(
      ids.iter().all(|id| *id == ids[0]),
      "{name} records were tied to more than one id: {ids:?}"
    );
    assert_eq!(
      ids[0], provider_id,
      "{name} persisted a different id than the provider supplied"
    );

    if !action_ids.contains(&ids[0]) {
      action_ids.push(ids[0]);
    }
  }

  // Two tool actions, two ids: an id must not tie two actions together.
  assert_eq!(action_ids.len(), 2, "actions shared an id: {action_ids:?}");
}

/// Per tool action: exactly one terminal record, and it is sequenced after the
/// record that started the action.
#[test]
fn each_tool_action_leaves_one_terminal_record_after_its_start() {
  let records = tool_turn_records();

  // The journal's own sequence is what orders a later reader.
  assert!(
    records.windows(2).all(|pair| pair[0].seq < pair[1].seq),
    "lifecycle sequences are not strictly increasing: {:?}",
    records.iter().map(|record| record.seq).collect::<Vec<_>>()
  );

  let mut ids: Vec<&str> = Vec::new();
  for record in &records {
    let id = call_id(&record.event).expect("lifecycle record carries a call id");
    if !ids.contains(&id) {
      ids.push(id);
    }
  }
  assert!(
    ids.len() > 1,
    "the turn performed one tool action, not more: {ids:?}"
  );

  for id in ids {
    let action: Vec<&Record> = records
      .iter()
      .filter(|record| call_id(&record.event) == Some(id))
      .collect();
    let starts: Vec<&&Record> = action
      .iter()
      .filter(|record| matches!(stage(&record.event), Some(Stage::Start)))
      .collect();
    let terminals: Vec<&&Record> = action
      .iter()
      .filter(|record| matches!(stage(&record.event), Some(Stage::Terminal)))
      .collect();

    assert_eq!(
      terminals.len(),
      1,
      "tool action {id} left {} terminal records: {terminals:?}",
      terminals.len(),
    );
    assert!(
      !starts.is_empty(),
      "tool action {id} recorded a terminal state without a start: {action:?}"
    );
    assert!(
      starts[0].seq < terminals[0].seq,
      "tool action {id} started at sequence {} but closed at sequence {}",
      starts[0].seq,
      terminals[0].seq,
    );
  }
}

/// A tool action that fails must close with a failure record, not a bare success.
#[test]
fn a_failing_tool_action_records_a_failure() {
  // `exit 3` is the deterministic failing action: the shell reports status 3.
  let records = turn_records(
    &[
      tool_call(FAILING_CALL, "exec", r#"{"command":"exit 3"}"#),
      text_response("completed"),
    ],
    2,
  );

  let action: Vec<&Record> = records
    .iter()
    .filter(|record| call_id(&record.event) == Some(FAILING_CALL))
    .collect();
  assert!(
    action.len() > 1,
    "the failing action left {action:?}, which is not a lifecycle"
  );
  let terminals: Vec<&&Record> = action
    .iter()
    .filter(|record| matches!(stage(&record.event), Some(Stage::Terminal)))
    .collect();
  assert_eq!(
    terminals.len(),
    1,
    "the failing action left {} terminal records: {terminals:?}",
    terminals.len(),
  );

  match &terminals[0].event {
    AgentEvent::ToolFailed(event) => {
      assert_eq!(event.name, "exec");
      assert_eq!(
        event.status,
        Some(3),
        "the failure record did not carry the exit status the command died with"
      );
      assert!(
        !event.message.trim().is_empty(),
        "the failure record carries no reason: {event:?}"
      );
    }
    AgentEvent::ToolCompleted(event) => {
      panic!("the failing action was recorded as a success: {event:?}");
    }
    other => panic!("the failing action closed with {other:?}, not a failure"),
  }
}

/// A command killed at its timeout boundary is an outcome the runtime cannot
/// determine. The journal must record it as unknown: the runtime may not pick
/// between success and failure for a command that could already have written state.
#[test]
fn an_outcome_the_runtime_cannot_determine_is_not_coerced_into_success_or_failure() {
  // Use a platform-native long-running shell command so this lifecycle test
  // exercises timeout semantics rather than assuming a Unix `sleep` binary.
  #[cfg(windows)]
  let timeout_command = r#"{"command":"ping -n 6 127.0.0.1 >nul","timeout_ms":250}"#;
  #[cfg(not(windows))]
  let timeout_command = r#"{"command":"sleep 5","timeout_ms":250}"#;
  let records = turn_records(
    &[
      tool_call(UNOBSERVED_CALL, "exec", timeout_command),
      text_response("completed"),
    ],
    2,
  );

  let action: Vec<&Record> = records
    .iter()
    .filter(|record| call_id(&record.event) == Some(UNOBSERVED_CALL))
    .collect();
  assert!(
    action.len() > 1,
    "the unobserved action left {action:?}, which is not a lifecycle"
  );
  let terminals: Vec<&&Record> = action
    .iter()
    .filter(|record| matches!(stage(&record.event), Some(Stage::Terminal)))
    .collect();
  assert_eq!(
    terminals.len(),
    1,
    "the unobserved action left {} terminal records: {terminals:?}",
    terminals.len(),
  );

  // The decided states belong to the coercion this test rules out, so neither may
  // appear anywhere in the action, terminal record or not.
  for record in &action {
    assert!(
      !matches!(
        record.event,
        AgentEvent::ToolCompleted(_) | AgentEvent::ToolFailed(_)
      ),
      "the runtime coerced an outcome it could not observe into a decided state: {:?}",
      record.event,
    );
  }

  match &terminals[0].event {
    AgentEvent::ToolUnknown(event) => {
      assert_eq!(event.name, "exec");
      assert!(
        event.mutating,
        "a killed command that may have changed the workspace was recorded as read-only"
      );
      // What the journal holds for this action is the boundary the runtime stopped
      // at, not a verdict: the command was killed and its completion was never
      // observed. The killed command is not named there, so the test does not
      // claim that it is.
      assert!(
        event.why.contains("killed") && event.why.contains("unknown"),
        "the unknown record does not name the boundary that was not observed: {:?}",
        event.why,
      );
    }
    other => panic!("the unobserved action closed with {other:?}, not an unknown"),
  }
}

/// Drive one real turn that performs two tool actions, then read the lifecycle
/// records back out of the store the way a later process would.
fn tool_turn_records() -> Vec<Record> {
  let temp = TempDir::new().expect("temp root");
  let workspace = temp.path().join("workspace");
  fs::create_dir(&workspace).expect("create workspace");
  #[cfg(windows)]
  let exec_cmd = r#"{"command":"set /p=executed<nul>exec.txt&exit /b 0"}"#;
  #[cfg(not(windows))]
  let exec_cmd = r#"{"command":"printf executed > exec.txt"}"#;

  let server = FakeServer::answer(vec![
    tool_call(
      WRITE_CALL,
      "write",
      r#"{"path":"model.txt","contents":"from tool\n"}"#,
    ),
    tool_call(EXEC_CALL, "exec", exec_cmd),
    text_response("completed"),
  ]);
  // Both tools are mutating, so the run needs explicit auto-approval to reach them.
  let config = write_config(temp.path(), &server.base_url(), true);
  let state = temp.path().join("state");

  let output = run(&config, &workspace, "make the files");
  let requests = server.requests();
  assert!(
    output.status.success(),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  // Two tool calls plus the closing answer is the shape of a multi-action turn.
  assert_eq!(requests.len(), 3, "the turn was not a multi-action turn");

  let records = lifecycle_records(&state);
  drop(temp);
  records
}

/// Drive one real turn with `scripted` provider responses, then read the lifecycle
/// records back out of the store the way a later process would. `requests` is the
/// number of model requests the script implies, which pins the shape of the turn.
fn turn_records(scripted: &[String], requests: usize) -> Vec<Record> {
  let temp = TempDir::new().expect("temp root");
  let workspace = temp.path().join("workspace");
  fs::create_dir(&workspace).expect("create workspace");
  let server = FakeServer::answer(scripted.to_vec());
  // The actions below use exec, which is mutating, so the run needs auto-approval
  // to reach it.
  let config = write_config(temp.path(), &server.base_url(), true);
  let state = temp.path().join("state");

  let output = run(&config, &workspace, "make the files");
  let observed = server.requests();
  assert!(
    output.status.success(),
    "{}",
    String::from_utf8_lossy(&output.stderr)
  );
  assert_eq!(
    observed.len(),
    requests,
    "the turn did not use the scripted number of model requests"
  );

  let records = lifecycle_records(&state);
  drop(temp);
  records
}

/// A lifecycle record and the sequence the store assigned it.
fn lifecycle_records(state: &Path) -> Vec<Record> {
  let layout = StateLayout::new(state);
  let session_id = layout
    .list_session_ids()
    .unwrap()
    .pop()
    .expect("session id");
  TraceJournal::read(&layout.trace_path(&session_id))
    .unwrap()
    .items
    .into_iter()
    .map(|entry| Record {
      seq: entry.envelope.meta.seq.expect("store sequence").0,
      event: entry.envelope.event,
    })
    .filter(|record| call_id(&record.event).is_some())
    .collect()
}

/// The durable tool-call id a lifecycle record carries, if it is one.
fn call_id(event: &AgentEvent) -> Option<&str> {
  let id = match event {
    AgentEvent::ToolRequested(event) => &event.call_id,
    AgentEvent::ToolStarted(event) => &event.call_id,
    AgentEvent::ToolCompleted(event) => &event.call_id,
    AgentEvent::ToolFailed(event) => &event.call_id,
    AgentEvent::ToolUnknown(event) => &event.call_id,
    _ => return None,
  };
  Some(id.as_str())
}

/// The tool an action record names, for grouping records by tool action.
fn tool_name(event: &AgentEvent) -> Option<&str> {
  match event {
    AgentEvent::ToolRequested(event) => Some(&event.name),
    AgentEvent::ToolStarted(event) => Some(&event.name),
    AgentEvent::ToolCompleted(event) => Some(&event.name),
    AgentEvent::ToolFailed(event) => Some(&event.name),
    AgentEvent::ToolUnknown(event) => Some(&event.name),
    _ => None,
  }
}

/// Whether a record opens or closes a tool action.
fn stage(event: &AgentEvent) -> Option<Stage> {
  match event {
    AgentEvent::ToolRequested(_) | AgentEvent::ToolStarted(_) => Some(Stage::Start),
    AgentEvent::ToolCompleted(_) | AgentEvent::ToolFailed(_) | AgentEvent::ToolUnknown(_) => {
      Some(Stage::Terminal)
    }
    _ => None,
  }
}

/// One lifecycle record as the journal stored it.
#[derive(Debug)]
struct Record {
  /// Store-assigned sequence: the durable order, not the reader's iteration order.
  seq: u64,
  event: AgentEvent,
}

/// Where a record sits in one tool action's lifecycle.
enum Stage {
  /// The action is open.
  Start,
  /// The action is closed.
  Terminal,
}

fn write_config(root: &Path, base_url: &str, auto_approve_mutating: bool) -> PathBuf {
  let state = root.join("state");
  let mut config = RuntimeConfig::new(ModelRef::new("fake", "agent"), state.to_string_lossy());
  config.endpoints.push(ModelEndpoint {
    provider: "fake".into(),
    model: "agent".into(),
    base_url: Some(base_url.into()),
    api_key_env: None,
    api_key: None,
    capabilities: ModelCapabilities {
      text: true,
      images: false,
      tools: true,
      exposed_reasoning: ReasoningExposure::Native,
      context_window: 32_768,
      max_output_tokens: Some(1_024),
    },
    max_output_tokens: Some(1_024),
    connect_timeout_ms: None,
    read_timeout_ms: None,
    request_timeout_ms: None,
  });
  config.tools.auto_approve_mutating = auto_approve_mutating;
  let path = root.join("config.json");
  fs::write(&path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
  path
}

fn run(config: &Path, cwd: &Path, prompt: &str) -> std::process::Output {
  Command::new(env!("CARGO_BIN_EXE_rupi"))
    .args(["run", "--config"])
    .arg(config)
    .arg("--cwd")
    .arg(cwd)
    .args(["--prompt", prompt])
    .output()
    .expect("run rupi")
}
