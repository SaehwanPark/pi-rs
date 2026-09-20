//! Phase 8 gate fixtures for the lazy Node extension host.

use std::path::PathBuf;

use rupi_compat::{compat, scan::Discovery};
use rupi_core::{CancelToken, ToolChunk, ToolExecutionState, ToolRequest};
use rupi_extension::{ExtensionHost, ExtensionHostConfig, HostError, HostStatus};
use rupi_tools::{AutoApprove, ToolRegistry, Workspace};
use serde_json::json;
use tempfile::TempDir;

fn fixture(name: &str) -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("tests/compat/extensions")
    .join(name)
}

fn node_is_available() {
  assert!(
    std::process::Command::new("node")
      .arg("--version")
      .output()
      .expect("invoke node")
      .status
      .success(),
    "Phase 8 fixtures require Node"
  );
}

#[test]
fn compatibility_fixture_reports_selected_extension_support() {
  let report = compat::inspect_target(
    fixture("package.json").to_str().expect("fixture path"),
    &Discovery::new("."),
  )
  .expect("inspect extension fixture");
  assert!(report.surfaces.iter().any(|surface| {
    surface.name == "tool registration" && surface.status == compat::CompatibilityLevel::Supported
  }));
  assert!(report.surfaces.iter().any(|surface| {
    surface.name == "command registration"
      && surface.status == compat::CompatibilityLevel::Supported
  }));
  assert!(report.surfaces.iter().any(|surface| {
    surface.name == "extensions" && surface.status == compat::CompatibilityLevel::Partial
  }));
}

#[test]
fn host_is_lazy_and_no_extensions_never_launch_node() {
  let host = ExtensionHost::new(ExtensionHostConfig::default());
  assert_eq!(host.status(), HostStatus::NotStarted);
  host.start().expect("empty host is a no-op");
  assert_eq!(host.status(), HostStatus::Ready);
  assert!(host.tools().is_empty());
  host
    .shutdown(json!({"reason": "quit"}))
    .expect("empty shutdown is a no-op");
  assert_eq!(host.status(), HostStatus::Stopped);
}

#[test]
fn pi_style_typescript_fixture_registers_selected_apis() {
  node_is_available();
  let host = ExtensionHost::new(ExtensionHostConfig::new([fixture("phase8-fixture.ts")]));
  assert_eq!(host.status(), HostStatus::NotStarted);
  host.start().expect("load fixture");
  assert_eq!(host.status(), HostStatus::Ready);
  assert_eq!(host.tools()[0].name, "fixture-greet");
  assert_eq!(host.commands()[0].name, "fixture-command");

  let result = host
    .dispatch_tool("fixture-greet", json!({"name": "Ada"}))
    .expect("tool dispatch");
  assert_eq!(result.text.as_deref(), Some("hello Ada"));

  let command = host
    .dispatch_command("fixture-command", json!("go"))
    .expect("command dispatch");
  assert!(
    command
      .ui
      .iter()
      .any(|event| { event.kind == "notify" && event.text.as_deref() == Some("command:go") })
  );

  let lifecycle = host
    .dispatch_event(
      rupi_extension::LifecycleEvent::SessionStart,
      json!({"reason": "startup"}),
    )
    .expect("lifecycle dispatch");
  assert_eq!(lifecycle.ui.len(), 3);

  let context = host
    .dispatch_context(json!({"messages": []}))
    .expect("context hook");
  assert_eq!(
    context.value.unwrap()["messages"][0]["content"],
    "extension context"
  );
  host
    .shutdown(json!({"reason": "quit"}))
    .expect("shutdown host");
  assert_eq!(host.status(), HostStatus::Stopped);
}

#[test]
fn process_loss_is_distinct_from_extension_failure() {
  node_is_available();
  let host = ExtensionHost::new(ExtensionHostConfig::new([fixture(
    "phase8-process-loss.mjs",
  )]));
  let error = host.start().expect_err("crashed host reports process loss");
  assert!(matches!(error, HostError::ProcessLost(_)));
  assert!(matches!(
    host.status(),
    HostStatus::Stopped | HostStatus::Failed
  ));
}

#[test]
fn relative_modules_resolve_against_the_configured_working_directory() {
  node_is_available();
  let host = ExtensionHost::new(
    ExtensionHostConfig::new(["phase8-fixture.ts"]).with_working_directory(fixture("")),
  );
  host.start().expect("relative fixture path loads");
  assert_eq!(host.tools()[0].name, "fixture-greet");
  host.stop();
}

#[test]
fn extension_errors_are_isolated_and_mutating_failure_is_unknown() {
  node_is_available();
  let host = ExtensionHost::new(ExtensionHostConfig::new([
    fixture("phase8-fixture.ts"),
    fixture("phase8-failure-fixture.ts"),
  ]));
  host.start().expect("load fixtures");

  let lifecycle_error = host
    .dispatch_event(rupi_extension::LifecycleEvent::TurnEnd, json!({}))
    .expect_err("failing extension hook is reported");
  assert!(matches!(lifecycle_error, HostError::Extension { .. }));
  assert_eq!(host.status(), HostStatus::Ready);

  let tool = host
    .tool_wrappers()
    .expect("wrappers remain available")
    .into_iter()
    .find(|tool| tool.info().name == "fixture-failing-tool")
    .expect("failing tool is registered");
  let request = ToolRequest {
    call_id: rupi_core::ids::ToolCallId::new(),
    name: tool.info().name.clone(),
    arguments: json!({}),
  };

  // The core registry retains its lifecycle invariant when the host reports an
  // extension failure: a mutating tool is Unknown, never an observed failure.
  let temp = TempDir::new().expect("temp workspace");
  let mut registry = ToolRegistry::new(Workspace::new(temp.path()).expect("workspace"));
  registry.register(Box::new(tool));
  let mut progress = |_chunk: &ToolChunk| {};
  let mut gate = AutoApprove;
  let executed = registry.execute_with(&request, &mut progress, &CancelToken::new(), &mut gate);
  assert_eq!(executed.state, ToolExecutionState::Unknown);
  assert!(
    executed
      .outcome
      .text
      .contains("completion was not observed")
  );
}
