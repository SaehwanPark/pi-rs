use std::{thread, time::Duration};

use pi_rs_core::{ModelRef, now_millis};
use pi_rs_mcp::protocol::{CallToolResult, JsonRpcRequest};
use pi_rs_mcp::{
  StartRequest, WorkerEngine, WorkerExecution, WorkerMcpServer, WorkerMessage, WorkerRunRequest,
  WorkerService, WorkerSummary, WorkerSummarySource, WorkerTraceEntry,
};
use serde_json::json;

#[derive(Debug, Default)]
struct FixtureEngine;

impl WorkerEngine for FixtureEngine {
  fn run(
    &self,
    request: WorkerRunRequest,
    cancel: pi_rs_mcp::WorkerCancelToken,
  ) -> Result<WorkerExecution, pi_rs_mcp::WorkerError> {
    if request.prompt == "cancel me" {
      while !cancel.is_cancelled() {
        thread::sleep(Duration::from_millis(2));
      }
      return Ok(WorkerExecution::default());
    }
    let model = ModelRef::new("fixture", "worker");
    Ok(WorkerExecution {
      summary: Some(WorkerSummary {
        text: format!("summary: {}", request.prompt),
        source: WorkerSummarySource::Declared,
        updated_at_ms: now_millis(),
      }),
      messages: vec![WorkerMessage {
        role: "assistant".into(),
        text: request.prompt,
        epoch: 0,
        model: model.clone(),
        seq: Some(1),
        external_context: None,
      }],
      trace: vec![WorkerTraceEntry {
        seq: 1,
        kind: "turn_completed".into(),
        timestamp_ms: now_millis(),
        epoch: Some(0),
        model: Some(model.clone()),
        provenance: Some("runtime".into()),
      }],
      active_model: Some(model),
      ..WorkerExecution::default()
    })
  }
}

#[test]
fn public_worker_boundary_supports_run_cancel_and_resource_reads() {
  let service = WorkerService::new(FixtureEngine);
  let handle = service
    .start(StartRequest {
      prompt: "hello from orchestrator".into(),
      wait_ms: Some(100),
    })
    .expect("start");
  assert_eq!(handle.status, pi_rs_mcp::WorkerStatus::Completed);
  let summary = service
    .read_resource(&format!("session://{}/summary", handle.session_id))
    .expect("summary");
  assert_eq!(summary["summary"]["source"], "declared");
  let trace = service
    .read_resource(&format!("session://{}/trace?limit=1", handle.session_id))
    .expect("trace");
  assert_eq!(trace["items"][0]["kind"], "turn_completed");

  let running = service
    .start(StartRequest {
      prompt: "cancel me".into(),
      wait_ms: None,
    })
    .expect("second start");
  let cancelled = service
    .cancel(pi_rs_mcp::CancelRequest {
      session_id: running.session_id,
      run_id: running.run_id,
      reason: Some("orchestrator stop".into()),
    })
    .expect("cancel");
  assert_eq!(cancelled.status, pi_rs_mcp::WorkerStatus::Cancelling);
}

#[test]
fn public_mcp_server_returns_coarse_tool_result_without_terminal_text() {
  let server = WorkerMcpServer::new(WorkerService::new(FixtureEngine));
  let response = server.handle(JsonRpcRequest::new(
    1,
    "tools/call",
    Some(json!({
      "name": "agent.start",
      "arguments": {"prompt": "mcp", "wait_ms": 100}
    })),
  ));
  assert!(response.error.is_none());
  let result: CallToolResult = serde_json::from_value(response.result.unwrap()).unwrap();
  assert_eq!(result.is_error, Some(false));
  let text = result.text_content();
  assert!(text.contains("session_id"));
  assert!(!text.contains("stdout"));
}

#[test]
fn resource_uri_rejects_path_traversal_and_unknown_projection() {
  let service = WorkerService::new(FixtureEngine);
  let traversal = service.read_resource("session://../trace").unwrap_err();
  assert_eq!(traversal.code, "invalid_params");
  let unknown = service
    .read_resource("session://session/unknown")
    .unwrap_err();
  assert_eq!(unknown.code, "resource_not_found");
}

#[test]
fn cancel_token_is_atomic_and_cloneable() {
  let token = pi_rs_mcp::WorkerCancelToken::default();
  let clone = token.clone();
  clone.cancel();
  assert!(token.is_cancelled());
}
