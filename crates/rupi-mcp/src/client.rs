//! High-level MCP client handling handshake, capabilities, and tool dispatch.

use std::{
  collections::HashSet,
  sync::{Arc, Mutex},
};

use rupi_core::ToolExecutionContext;

use serde_json::{Value, json};

const MAX_TOOL_LIST_PAGES: usize = 1_024;
const MAX_CURSOR_BYTES: usize = 8 * 1024;

use crate::{
  error::McpError,
  protocol::{
    CallToolParams, CallToolResult, ClientCapabilities, ClientInfo, InitializeParams,
    InitializeResult, LATEST_PROTOCOL_VERSION, ListToolsResult, MAX_MCP_TOOLS_PER_SERVER,
    McpToolDefinition, ServerCapabilities, ServerInfo, negotiate_protocol_version,
  },
  transport::McpTransport,
};

/// High-level MCP client.
pub struct McpClient {
  transport: Arc<dyn McpTransport>,
  server_info: Mutex<Option<ServerInfo>>,
  server_capabilities: Mutex<Option<ServerCapabilities>>,
  negotiated_version: Mutex<Option<String>>,
}

impl McpClient {
  /// Create a client wrapping an MCP transport.
  pub fn new(transport: Arc<dyn McpTransport>) -> Self {
    Self {
      transport,
      server_info: Mutex::new(None),
      server_capabilities: Mutex::new(None),
      negotiated_version: Mutex::new(None),
    }
  }

  /// Perform the initialization handshake with protocol negotiation.
  pub fn initialize(&self) -> Result<InitializeResult, McpError> {
    let params = InitializeParams {
      protocol_version: LATEST_PROTOCOL_VERSION.to_string(),
      // Roots are intentionally omitted until this client can dispatch the
      // server-initiated `roots/list` request. Advertising an unsupported
      // capability makes an otherwise compatible server hang on handshake.
      capabilities: ClientCapabilities {
        roots: None,
        sampling: None,
      },
      client_info: ClientInfo {
        name: "rupi".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
      },
    };

    let params_val = serde_json::to_value(params)
      .map_err(|e| McpError::Protocol(format!("failed to serialize initialize params: {e}")))?;

    let res_val = self.transport.call("initialize", Some(params_val))?;
    let init_result: InitializeResult = serde_json::from_value(res_val)
      .map_err(|e| McpError::Protocol(format!("invalid initialize result from MCP server: {e}")))?;

    let negotiated =
      negotiate_protocol_version(&init_result.protocol_version).ok_or_else(|| {
        McpError::NegotiationFailed(format!(
          "unsupported MCP protocol version '{}' from server",
          init_result.protocol_version
        ))
      })?;

    *self.negotiated_version.lock().unwrap() = Some(negotiated.to_string());
    self.transport.set_protocol_version(negotiated);

    // Handshake completion notification
    self.transport.notify("notifications/initialized", None)?;

    *self.server_info.lock().unwrap() = Some(init_result.server_info.clone());
    *self.server_capabilities.lock().unwrap() = Some(init_result.capabilities.clone());

    Ok(init_result)
  }

  /// Discover tools exposed by this MCP server.
  pub fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpError> {
    let mut all_tools = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen_cursors = HashSet::new();

    for page_index in 0..MAX_TOOL_LIST_PAGES {
      let params = cursor.as_ref().map(|c| json!({ "cursor": c }));
      let res_val = self.transport.call("tools/list", params)?;
      let page: ListToolsResult = serde_json::from_value(res_val)
        .map_err(|e| McpError::Protocol(format!("invalid tools/list response: {e}")))?;

      if all_tools.len().saturating_add(page.tools.len()) > MAX_MCP_TOOLS_PER_SERVER {
        return Err(McpError::Protocol(format!(
          "MCP tools/list exceeded the {MAX_MCP_TOOLS_PER_SERVER}-tool server limit"
        )));
      }
      all_tools.extend(page.tools);
      if let Some(next) = page.next_cursor {
        if next.len() > MAX_CURSOR_BYTES {
          return Err(McpError::Protocol(
            "MCP tools/list cursor is too large".into(),
          ));
        }
        if !next.is_empty() {
          if !seen_cursors.insert(next.clone()) {
            return Err(McpError::Protocol(format!(
              "MCP tools/list repeated cursor after page {}",
              page_index + 1
            )));
          }
          cursor = Some(next);
          continue;
        }
      }
      return Ok(all_tools);
    }

    Err(McpError::Protocol(format!(
      "MCP tools/list exceeded {MAX_TOOL_LIST_PAGES} pages"
    )))
  }

  /// Call an MCP tool on the server.
  pub fn call_tool(
    &self,
    name: &str,
    arguments: Option<Value>,
  ) -> Result<CallToolResult, McpError> {
    self.call_tool_with_context(name, arguments, &ToolExecutionContext::unbounded())
  }

  pub fn call_tool_with_context(
    &self,
    name: &str,
    arguments: Option<Value>,
    context: &ToolExecutionContext,
  ) -> Result<CallToolResult, McpError> {
    let params = CallToolParams {
      name: name.to_string(),
      arguments,
    };

    let params_val = serde_json::to_value(params)
      .map_err(|e| McpError::Protocol(format!("failed to serialize call params: {e}")))?;

    let res_val = self
      .transport
      .call_with_context("tools/call", Some(params_val), context)?;
    let call_res: CallToolResult = serde_json::from_value(res_val)
      .map_err(|e| McpError::Protocol(format!("invalid tools/call response: {e}")))?;

    Ok(call_res)
  }

  /// Cached server info if initialized.
  pub fn server_info(&self) -> Option<ServerInfo> {
    self.server_info.lock().unwrap().clone()
  }

  /// Cached server capabilities if initialized.
  pub fn server_capabilities(&self) -> Option<ServerCapabilities> {
    self.server_capabilities.lock().unwrap().clone()
  }

  /// Negotiated protocol version if initialized.
  pub fn negotiated_version(&self) -> Option<String> {
    self.negotiated_version.lock().unwrap().clone()
  }

  /// Whether the transport is alive.
  pub fn is_alive(&self) -> bool {
    self.transport.is_alive()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::transport::MockTransport;

  #[test]
  fn repeated_tools_cursor_is_rejected() {
    let mock = Arc::new(MockTransport::new());
    mock.on(
      "tools/list",
      json!({
        "tools": [],
        "nextCursor": "same"
      }),
    );
    let client = McpClient::new(mock);
    let error = client.list_tools().unwrap_err();
    assert!(format!("{error}").contains("repeated cursor"));
  }

  #[test]
  fn tool_catalog_count_is_bounded_before_manager_admission() {
    let mock = Arc::new(MockTransport::new());
    let tools: Vec<_> = (0..=MAX_MCP_TOOLS_PER_SERVER)
      .map(|index| json!({"name": format!("tool_{index}"), "inputSchema": {"type":"object"}}))
      .collect();
    mock.on("tools/list", json!({"tools": tools}));
    let client = McpClient::new(mock);

    let error = client
      .list_tools()
      .expect_err("oversized catalogs are refused while being discovered");
    assert!(error.to_string().contains("server limit"));
  }

  #[test]
  fn test_client_handshake_and_tools() {
    let mock = Arc::new(MockTransport::new());
    mock.on(
      "initialize",
      json!({
        "protocolVersion": "2024-11-05",
        "capabilities": {
          "tools": {}
        },
        "serverInfo": {
          "name": "test-server",
          "version": "1.0.0"
        }
      }),
    );
    mock.on(
      "tools/list",
      json!({
        "tools": [
          {
            "name": "test_search",
            "description": "search something",
            "inputSchema": {
              "type": "object",
              "properties": {
                "query": { "type": "string" }
              }
            }
          }
        ]
      }),
    );
    mock.on(
      "tools/call",
      json!({
        "content": [
          { "type": "text", "text": "found 42 results" }
        ],
        "isError": false
      }),
    );

    let client = McpClient::new(mock.clone());
    let init = client.initialize().expect("initialization succeeds");
    assert_eq!(init.server_info.name, "test-server");
    assert_eq!(client.negotiated_version().unwrap(), "2024-11-05");

    let tools = client.list_tools().expect("list tools succeeds");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "test_search");

    let call = client
      .call_tool("test_search", Some(json!({"query": "rust"})))
      .expect("call succeeds");
    assert_eq!(call.text_content(), "found 42 results");
  }
}
