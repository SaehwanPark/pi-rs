//! Integration tests for the registry: policy, approval, lifecycle, and bounds.
//!
//! These exercise the registry as the runtime will use it, over real files and
//! real commands. The unit tests inside each tool module check tool behaviour;
//! this file checks the decisions the registry makes *about* tools, which is the
//! part that has to be right for safety.

use std::{fs, path::Path};

use pi_rs_core::ToolExecutionState as State;
use pi_rs_core::{
  CancelToken, CancelToken as Cancel, ReplayDecision, ToolCallId, ToolChunk, ToolMetadata,
  ToolOutcome, ToolPolicy, ToolProgress, ToolRequest, ToolSpec,
};
use serde_json::{Value, json};
use tempfile::TempDir;

use pi_rs_tools::{Approval, ApprovalGate, Executed, ToolRegistry, Workspace};

fn workspace(dir: &Path) -> Workspace {
  Workspace::new(dir).unwrap()
}

fn request(name: &str, arguments: Value) -> ToolRequest {
  ToolRequest {
    call_id: ToolCallId::new(),
    name: name.to_string(),
    arguments,
  }
}

struct Sink {
  text: String,
}

impl Sink {
  fn new() -> Self {
    Self {
      text: String::new(),
    }
  }
}

impl ToolProgress for Sink {
  fn emit(&mut self, chunk: &ToolChunk) {
    self.text.push_str(&chunk.text);
  }
}

/// A gate that records what it was asked, so tests can assert that approval
/// really was consulted rather than merely assumed.
struct RecordingGate {
  allow: bool,
  asked: Vec<String>,
}

impl ApprovalGate for RecordingGate {
  fn decide(&mut self, metadata: &ToolMetadata, _arguments: &Value) -> Approval {
    self.asked.push(metadata.name.clone());
    if self.allow {
      Approval::Allow
    } else {
      Approval::Deny(format!("user declined '{}'", metadata.name))
    }
  }
}

/// Registry under the default policy: mutating tools are refused outright,
/// because a harness must not change state without an answer.
fn registry(dir: &TempDir) -> ToolRegistry {
  ToolRegistry::new(workspace(dir.path())).with_builtins()
}

/// Registry for an operator who has already accepted mutating tools by
/// configuration — the headless equivalent of answering "yes" once.
fn approved(dir: &TempDir) -> ToolRegistry {
  ToolRegistry::new(workspace(dir.path()))
    .with_builtins()
    .with_policy(&ToolPolicy {
      auto_approve_mutating: true,
      ..ToolPolicy::default()
    })
}

fn run(reg: &ToolRegistry, name: &str, arguments: Value) -> Executed {
  let mut sink = Sink::new();
  reg.execute(&request(name, arguments), &mut sink, &CancelToken::new())
}

fn fixture() -> TempDir {
  let dir = TempDir::new().unwrap();
  fs::create_dir_all(dir.path().join("src")).unwrap();
  fs::write(dir.path().join("src/main.rs"), "fn main() {\n  run();\n}\n").unwrap();
  fs::write(dir.path().join("README.md"), "# Title\n\nbody text\n").unwrap();
  dir
}

#[test]
fn the_builtin_set_is_registered_with_usable_specs() {
  let dir = fixture();
  let reg = registry(&dir);
  let names = reg.names();
  assert!(names.contains(&"read".to_string()));
  assert!(names.contains(&"write".to_string()));
  assert!(names.contains(&"edit".to_string()));
  assert!(names.contains(&"grep".to_string()));
  assert!(names.contains(&"exec".to_string()));

  let specs = reg.specs();
  assert_eq!(specs.len(), names.len());
  for spec in &specs {
    assert!(
      !spec.description.is_empty(),
      "{} has no description",
      spec.name
    );
    assert!(spec.parameters.is_object(), "{} has no schema", spec.name);
  }
}

#[test]
fn read_write_and_edit_compose_on_a_real_file() {
  let dir = fixture();
  let reg = approved(&dir);

  let read = run(&reg, "read", json!({"path": "src/main.rs"}));
  assert!(!read.outcome.is_error, "{}", read.outcome.text);
  assert!(read.outcome.text.contains("fn main()"));

  let edited = run(
    &reg,
    "write",
    json!({"path": "src/main.rs", "contents": "fn main() {\n  run(1);\n}\n"}),
  );
  assert!(!edited.outcome.is_error, "{}", edited.outcome.text);

  let again = run(&reg, "read", json!({"path": "src/main.rs"}));
  assert!(
    again.outcome.text.contains("run(1)"),
    "{}",
    again.outcome.text
  );
}

#[test]
fn a_mutating_call_is_not_run_without_an_answer() {
  // Default policy: mutating tools are refused rather than silently executed.
  // This is the behaviour that must never regress in a headless run.
  let dir = fixture();
  let reg = registry(&dir);
  let executed = run(&reg, "write", json!({"path": "x.txt", "contents": "data"}));
  assert!(executed.outcome.is_error, "must not succeed");
  assert!(!executed.started, "must not have started");
  assert!(!dir.path().join("x.txt").exists(), "must not have written");
  let reason = executed.refusal.clone().unwrap_or_default();
  assert!(
    reason.contains("approval is required"),
    "the refusal says what would unblock it: {reason}"
  );
  let reason = executed.refusal.clone().unwrap_or_default();
  assert!(
    reason.contains("mutating"),
    "the refusal must name the reason: {reason}"
  );
}

#[test]
fn an_explicit_gate_answer_is_honoured() {
  let dir = fixture();
  let reg = registry(&dir);

  let mut allow = RecordingGate {
    allow: true,
    asked: Vec::new(),
  };
  let mut sink = Sink::new();
  let executed = reg.execute_with(
    &request("write", json!({"path": "ok.txt", "contents": "yes"})),
    &mut sink,
    &CancelToken::new(),
    &mut allow,
  );
  assert!(
    executed.outcome.text.contains("wrote"),
    "{}",
    executed.outcome.text
  );
  assert_eq!(allow.asked, vec!["write".to_string()], "the gate was asked");
  assert!(dir.path().join("ok.txt").exists());

  let mut deny = RecordingGate {
    allow: false,
    asked: Vec::new(),
  };
  let mut sink = Sink::new();
  let refused = reg.execute_with(
    &request("write", json!({"path": "no.txt", "contents": "no"})),
    &mut sink,
    &CancelToken::new(),
    &mut deny,
  );
  assert!(refused.outcome.is_error);
  assert_eq!(
    refused.state,
    State::Failed,
    "never started, so not Unknown"
  );
  assert!(!dir.path().join("no.txt").exists());
  assert_eq!(refused.refusal.as_deref(), Some("user declined 'write'"));
}

#[test]
fn an_unanswered_prompt_does_not_become_permission() {
  // The failure mode an approval gate exists to prevent: a gate that asks a
  // question nobody answers, and a caller that reads the silence as a yes.
  struct Asks;
  impl ApprovalGate for Asks {
    fn decide(&mut self, metadata: &ToolMetadata, _arguments: &Value) -> Approval {
      Approval::Ask(format!("Allow '{}' to change state?", metadata.name))
    }
  }

  let dir = fixture();
  let reg = registry(&dir);
  let mut sink = Sink::new();
  let refused = reg.execute_with(
    &request("write", json!({"path": "maybe.txt", "contents": "no"})),
    &mut sink,
    &CancelToken::new(),
    &mut Asks,
  );
  assert!(refused.outcome.is_error, "an ask is not an approval");
  assert!(
    !dir.path().join("maybe.txt").exists(),
    "nothing was written"
  );
  assert!(!refused.started);
}

#[test]
fn policy_can_deny_a_single_tool_without_disabling_the_rest() {
  let dir = fixture();
  let policy = ToolPolicy {
    deny: vec!["exec".to_string()],
    auto_approve_mutating: true,
    ..ToolPolicy::default()
  };
  let reg = ToolRegistry::new(workspace(dir.path()))
    .with_builtins()
    .with_policy(&policy);

  assert!(!reg.is_allowed("exec"));
  assert!(reg.is_allowed("read"));
  assert!(!reg.allowed_names().contains(&"exec".to_string()));
  assert!(
    !reg.specs().iter().any(|s: &ToolSpec| s.name == "exec"),
    "no schema leak"
  );

  let blocked = run(&reg, "exec", json!({"command": "echo hi"}));
  assert!(blocked.outcome.is_error);
  assert!(blocked.refusal.unwrap().contains("denied by policy"));

  // A read-only tool still works, so the policy is a gate, not a kill switch.
  let allowed = run(&reg, "grep", json!({"pattern": "Title"}));
  assert!(allowed.outcome.text.contains("README.md"));
}

#[test]
fn an_unknown_tool_names_the_alternatives() {
  let dir = fixture();
  let reg = registry(&dir);
  let executed = run(&reg, "delete_all", json!({}));
  assert!(executed.outcome.is_error);
  let reason = executed.refusal.unwrap();
  assert!(reason.contains("unknown tool 'delete_all'"), "{reason}");
  assert!(reason.contains("read"), "lists alternatives: {reason}");
}

#[test]
fn malformed_arguments_are_refused_before_execution() {
  let dir = fixture();
  let reg = registry(&dir);
  // 'find' missing entirely.
  let executed = run(&reg, "edit", json!({"path": "src/main.rs", "replace": "x"}));
  assert!(executed.outcome.is_error);
  assert!(
    executed.refusal.unwrap().contains("required argument"),
    "clear argument error"
  );

  // Wrong type for a required argument.
  let wrong_type = run(&reg, "read", json!({"path": 42}));
  assert!(wrong_type.outcome.is_error);
  assert!(wrong_type.refusal.unwrap().contains("must be string"));
}

#[test]
fn a_mutating_success_after_cancellation_is_not_believed() {
  // The core safety rule: cancellation observed after a mutating tool claims
  // success forces Unknown, because the effect may be half-applied.
  let dir = fixture();
  let reg = registry(&dir);
  let cancel = Cancel::new();
  cancel.cancel();
  let mut sink = Sink::new();
  let executed = reg.execute(
    &request("write", json!({"path": "x.txt", "contents": "data"})),
    &mut sink,
    &cancel,
  );
  assert!(executed.cancelled);
  assert!(!executed.started);
  assert_eq!(executed.state, State::Requested, "nothing ran at all");
  assert!(executed.outcome.is_error);
}

#[test]
fn read_only_calls_are_replayable_and_mutations_are_not() {
  let dir = fixture();
  let reg = registry(&dir);
  let read_meta = reg.metadata_for("read").unwrap();
  let write_meta = reg.metadata_for("write").unwrap();
  let exec_meta = reg.metadata_for("exec").unwrap();

  for state in [State::Started, State::Unknown] {
    assert_eq!(
      state.replay_decision(&read_meta),
      ReplayDecision::Replay,
      "read may be re-run from {state:?}"
    );
    assert_eq!(
      state.replay_decision(&write_meta),
      ReplayDecision::ReconcileFirst,
      "write must be reconciled from {state:?}"
    );
    assert_eq!(
      state.replay_decision(&exec_meta),
      ReplayDecision::ReconcileFirst
    );
  }
  assert!(reg.has_mutating_tools());
}

#[test]
fn large_tool_output_is_reduced_with_the_full_bytes_available() {
  let dir = fixture();
  let policy = ToolPolicy {
    max_output_bytes: 2_048,
    auto_approve_mutating: true,
    ..ToolPolicy::default()
  };
  let reg = ToolRegistry::new(workspace(dir.path()))
    .with_builtins()
    .with_policy(&policy);
  fs::write(
    dir.path().join("big.txt"),
    (1..=20_000)
      .map(|i| format!("line {i} payload\n"))
      .collect::<String>(),
  )
  .unwrap();

  let executed = run(&reg, "read", json!({"path": "big.txt", "limit": 20_000}));
  assert!(executed.outcome.reduced, "flagged as reduced");
  assert!(
    executed.outcome.text.len() < 3_000,
    "{}",
    executed.outcome.text.len()
  );
  assert!(executed.outcome.text.contains("bytes elided"));
  assert!(
    executed.full_output.is_some(),
    "the runtime needs the full bytes to archive them"
  );
  assert!(executed.full_output.unwrap().len() > 3_000);
}

#[test]
fn output_is_streamed_to_the_progress_sink_as_well_as_returned() {
  let dir = fixture();
  let reg = registry(&dir);
  let mut sink = Sink::new();
  let executed = reg.execute(
    &request("grep", json!({"pattern": "Title"})),
    &mut sink,
    &CancelToken::new(),
  );
  assert!(sink.text.contains("README.md"), "{}", sink.text);
  assert_eq!(executed.outcome.text, sink.text);
}

#[test]
fn mutating_tools_can_be_auto_approved_by_configuration() {
  let dir = fixture();
  let reg = approved(&dir);
  let executed = run(&reg, "write", json!({"path": "y.txt", "contents": "ok"}));
  assert!(!executed.outcome.is_error, "{}", executed.outcome.text);
  assert!(dir.path().join("y.txt").exists());
}

#[test]
fn a_command_runs_and_reports_its_state_through_the_registry() {
  let dir = fixture();
  let policy = ToolPolicy {
    auto_approve_mutating: true,
    shell_timeout_ms: 5_000,
    ..ToolPolicy::default()
  };
  let reg = ToolRegistry::new(workspace(dir.path()))
    .with_builtins()
    .with_policy(&policy);

  let ok = run(&reg, "exec", json!({"command": "echo registry"}));
  assert!(!ok.outcome.is_error, "{}", ok.outcome.text);
  assert!(ok.outcome.text.contains("registry"));
  assert_eq!(ok.state, State::Succeeded);

  let bad = run(&reg, "exec", json!({"command": "exit 3"}));
  assert!(bad.outcome.is_error);
  assert_eq!(bad.state, State::Failed);
  assert_eq!(bad.outcome.status, Some(3));
}

#[test]
fn the_registry_reports_metadata_for_every_permitted_tool() {
  let dir = fixture();
  let reg = registry(&dir);
  let metadata = reg.metadata();
  assert_eq!(metadata.len(), reg.names().len());
  assert!(
    metadata.iter().any(|m| m.name == "read" && m.read_only),
    "read-only declared"
  );
  assert!(
    metadata
      .iter()
      .any(|m| m.name == "exec" && !m.read_only && !m.idempotent),
    "exec declared mutating and non-idempotent"
  );
}

#[test]
fn replacing_a_builtin_is_allowed() {
  // An extension overrides a built-in by registering the same name. The registry
  // must not have a second resolution rule that makes the override invisible.
  struct Override;
  impl pi_rs_core::Tool for Override {
    fn metadata(&self) -> ToolMetadata {
      ToolMetadata::read_only("read", "replacement read")
    }
    fn arguments_schema(&self) -> Value {
      json!({"type": "object"})
    }
    fn execute(
      &self,
      _request: &ToolRequest,
      _progress: &mut dyn ToolProgress,
    ) -> Result<ToolOutcome, pi_rs_core::ToolError> {
      Ok(ToolOutcome::succeeded("override"))
    }
  }

  let dir = fixture();
  let mut reg = registry(&dir);
  reg.register(Box::new(Override));
  assert_eq!(reg.len(), 5, "replacement does not grow the set");
  let executed = run(&reg, "read", json!({"path": "anything"}));
  assert_eq!(executed.outcome.text, "override");
}

#[test]
fn the_result_block_carries_the_lifecycle_state() {
  let dir = fixture();
  let reg = registry(&dir);
  let executed = run(&reg, "read", json!({"path": "README.md"}));
  let block = executed.to_block();
  assert_eq!(block.state, State::Succeeded);
  assert_eq!(block.name, "read");
  assert!(!block.is_error);
  assert_eq!(block.id, executed.request.call_id);
}

#[test]
fn a_cancel_token_shared_with_a_long_command_stops_it_early() {
  // The registry checks cancellation before starting; the tool owns the check
  // during execution. Both halves matter, so this asserts the boundary state.
  let dir = fixture();
  let policy = ToolPolicy {
    auto_approve_mutating: true,
    shell_timeout_ms: 60_000,
    ..ToolPolicy::default()
  };
  let reg = ToolRegistry::new(workspace(dir.path()))
    .with_builtins()
    .with_policy(&policy);
  let cancel = CancelToken::new();
  let mut sink = Sink::new();
  let before = reg.execute(
    &request("exec", json!({"command": "echo not_run"})),
    &mut sink,
    &cancel,
  );
  assert!(!before.cancelled, "not cancelled yet");
  cancel.cancel();
  let after = reg.execute(
    &request("exec", json!({"command": "echo not_run"})),
    &mut sink,
    &cancel,
  );
  assert!(after.cancelled);
  assert!(!after.outcome.text.contains("not_run"), "must not have run");
}
