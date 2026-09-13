use std::{collections::BTreeMap, sync::Arc};

use pi_rs_core::{
  CancelToken, ReplayDecision, ToolChunk, ToolExecutionState, ToolProgress, ToolRequest,
  ids::ToolCallId,
};
use pi_rs_mcp::{
  McpClient, McpManager, McpServerConfig, McpTool, McpToolDefinition, MockTransport, StdioTransport,
};
use pi_rs_tools::{ToolRegistry, Workspace};
use serde_json::json;

struct ChunkCollector(Vec<String>);
impl ToolProgress for ChunkCollector {
  fn emit(&mut self, chunk: &ToolChunk) {
    self.0.push(chunk.text.clone());
  }
}

#[test]
fn mcp_tools_normalize_into_registry_and_run_lifecycle() {
  let mock = Arc::new(MockTransport::new());
  mock.on(
    "tools/call",
    json!({
      "content": [
        { "type": "text", "text": "discovered 3 references in rkb" }
      ],
      "isError": false
    }),
  );

  let client = Arc::new(McpClient::new(mock));
  let tool_def = McpToolDefinition {
    name: "search".into(),
    description: Some("Search RKB knowledge graph".into()),
    input_schema: json!({
      "type": "object",
      "properties": {
        "query": { "type": "string" }
      },
      "required": ["query"]
    }),
  };

  let mcp_tool = McpTool::new("rkb", tool_def, client, true);

  let temp_dir = tempfile::tempdir().unwrap();
  let workspace = Workspace::new(temp_dir.path()).unwrap();
  let mut registry = ToolRegistry::new(workspace);

  // Register MCP tool alongside builtins
  registry.register(Box::new(mcp_tool));

  // Verify tool shows up in specs
  let specs = registry.specs();
  let mcp_spec = specs
    .iter()
    .find(|s| s.name == "mcp__rkb__search")
    .expect("MCP tool appears in registry specs");
  assert!(mcp_spec.description.contains("[MCP:rkb]"));

  // Execute tool through registry dispatch
  let request = ToolRequest {
    call_id: ToolCallId::new(),
    name: "mcp__rkb__search".into(),
    arguments: json!({ "query": "event trace" }),
  };

  let mut collector = ChunkCollector(Vec::new());
  let cancel = CancelToken::new();
  let executed = registry.execute(&request, &mut collector, &cancel);

  assert_eq!(executed.state, ToolExecutionState::Succeeded);
  assert_eq!(executed.outcome.text, "discovered 3 references in rkb");
  assert_eq!(collector.0, vec!["discovered 3 references in rkb"]);
  assert_eq!(executed.to_block().text, "discovered 3 references in rkb");
}

#[test]
fn mutating_mcp_tool_preserves_honest_uncertainty_under_pipe_failure() {
  let mock = Arc::new(MockTransport::new());
  // No response registered for tools/call -> triggers transport error
  let client = Arc::new(McpClient::new(mock));
  let tool_def = McpToolDefinition {
    name: "mutate_item".into(),
    description: Some("Mutate an item in external database".into()),
    input_schema: json!({ "type": "object" }),
  };

  // Mutating tool (read_only: false)
  let mcp_tool = McpTool::new("db", tool_def, client, false);

  let temp_dir = tempfile::tempdir().unwrap();
  let workspace = Workspace::new(temp_dir.path()).unwrap();
  let mut registry = ToolRegistry::new(workspace);
  registry.register(Box::new(mcp_tool));

  let request = ToolRequest {
    call_id: ToolCallId::new(),
    name: "mcp__db__mutate_item".into(),
    arguments: json!({ "id": 1 }),
  };

  let mut collector = ChunkCollector(Vec::new());
  let cancel = CancelToken::new();
  let mut gate = pi_rs_tools::AutoApprove;
  let executed = registry.execute_with(&request, &mut collector, &cancel, &mut gate);

  // Invariant verification: mutating tool in an unobserved state MUST report Unknown
  assert_eq!(executed.state, ToolExecutionState::Unknown);
  let metadata = registry.metadata_for("mcp__db__mutate_item").unwrap();
  assert_eq!(
    executed.replay_decision(&metadata),
    ReplayDecision::ReconcileFirst
  );
}

#[test]
fn real_subprocess_stdio_transport_wire_test() {
  // A tiny shell script running as an MCP server responding to initialize,
  // notifications/initialized, tools/list, and tools/call
  let script = r#"
while IFS= read -r line; do
  case "$line" in
    *"initialize"*)
      echo '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"mock-sh-mcp","version":"0.1.0"}}}'
      ;;
    *"tools/list"*)
      echo '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo","description":"echo text","inputSchema":{"type":"object"}}]}}'
      ;;
    *"tools/call"*)
      echo '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"echoed: hello wire"}],"isError":false}}'
      ;;
  esac
done
"#;

  let transport = StdioTransport::spawn(
    "sh",
    &["-c".to_string(), script.to_string()],
    &BTreeMap::new(),
  )
  .expect("spawns sh subprocess");

  let client = Arc::new(McpClient::new(Arc::new(transport)));
  let init = client.initialize().expect("initialize handshake succeeds");
  assert_eq!(init.server_info.name, "mock-sh-mcp");
  assert_eq!(client.negotiated_version().unwrap(), "2024-11-05");

  let tools = client.list_tools().expect("list tools succeeds");
  assert_eq!(tools.len(), 1);
  assert_eq!(tools[0].name, "echo");

  let call = client
    .call_tool("echo", Some(json!({"msg": "hello wire"})))
    .expect("call succeeds");
  assert_eq!(call.text_content(), "echoed: hello wire");
}

#[test]
fn manager_handles_multiple_servers_and_status() {
  let cfg1 = McpServerConfig::new("rkb", "rkb-bin").with_enabled(false);
  let cfg2 = McpServerConfig::new("github", "gh-mcp").with_enabled(true);

  let manager = McpManager::new(vec![cfg1, cfg2]);
  let statuses = manager.statuses();
  assert_eq!(statuses.len(), 2);

  let rkb = statuses.iter().find(|s| s.name == "rkb").unwrap();
  assert!(!rkb.active);
  assert!(!rkb.enabled);

  let gh = statuses.iter().find(|s| s.name == "github").unwrap();
  assert!(!gh.active);
  assert!(gh.enabled);
}
