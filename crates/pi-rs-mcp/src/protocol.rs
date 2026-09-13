//! MCP protocol definitions and JSON-RPC 2.0 wire types.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Current MCP protocol version supported by pi-rs.
pub const LATEST_PROTOCOL_VERSION: &str = "2024-11-05";

/// Supported MCP protocol versions for negotiation, newest first.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2024-11-05", "2024-10-07"];

/// JSON-RPC 2.0 Request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
  pub jsonrpc: String,
  pub id: Value,
  pub method: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub params: Option<Value>,
}

impl JsonRpcRequest {
  pub fn new(id: u64, method: impl Into<String>, params: Option<Value>) -> Self {
    Self {
      jsonrpc: "2.0".to_string(),
      id: Value::from(id),
      method: method.into(),
      params,
    }
  }
}

/// JSON-RPC 2.0 Notification (no id, expects no response).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
  pub jsonrpc: String,
  pub method: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub params: Option<Value>,
}

impl JsonRpcNotification {
  pub fn new(method: impl Into<String>, params: Option<Value>) -> Self {
    Self {
      jsonrpc: "2.0".to_string(),
      method: method.into(),
      params,
    }
  }
}

/// JSON-RPC 2.0 Response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
  pub jsonrpc: String,
  pub id: Value,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub result: Option<Value>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub error: Option<JsonRpcErrorObject>,
}

/// JSON-RPC 2.0 Error object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JsonRpcErrorObject {
  pub code: i64,
  pub message: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub data: Option<Value>,
}

/// Client info sent in `initialize`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
  pub name: String,
  pub version: String,
}

/// Client capabilities declared during handshake.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub roots: Option<Value>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub sampling: Option<Value>,
}

/// Parameters for `initialize` request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
  pub protocol_version: String,
  pub capabilities: ClientCapabilities,
  pub client_info: ClientInfo,
}

/// Server info received in `initialize` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
  pub name: String,
  #[serde(default)]
  pub version: Option<String>,
}

/// Server capabilities received in `initialize` response.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilities {
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub tools: Option<Value>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub resources: Option<Value>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub prompts: Option<Value>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub logging: Option<Value>,
}

/// Result returned from `initialize` call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
  pub protocol_version: String,
  pub capabilities: ServerCapabilities,
  pub server_info: ServerInfo,
}

/// Definition of an MCP tool returned by `tools/list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolDefinition {
  pub name: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub description: Option<String>,
  #[serde(default)]
  pub input_schema: Value,
}

/// Result of `tools/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListToolsResult {
  pub tools: Vec<McpToolDefinition>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub next_cursor: Option<String>,
}

/// Parameter for `tools/call`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallToolParams {
  pub name: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub arguments: Option<Value>,
}

/// Content piece in a `tools/call` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpContent {
  #[serde(rename = "type")]
  pub content_type: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub text: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub data: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub mime_type: Option<String>,
}

/// Result returned by `tools/call`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CallToolResult {
  #[serde(default)]
  pub content: Vec<McpContent>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub is_error: Option<bool>,
}

impl CallToolResult {
  /// Extract combined text from all text content chunks.
  pub fn text_content(&self) -> String {
    let mut out = String::new();
    for c in &self.content {
      if let Some(t) = &c.text {
        if !out.is_empty() {
          out.push('\n');
        }
        out.push_str(t);
      }
    }
    out
  }
}

/// Negotiate the protocol version between client and server.
pub fn negotiate_protocol_version(server_version: &str) -> Option<&'static str> {
  for &supported in SUPPORTED_PROTOCOL_VERSIONS {
    if server_version == supported {
      return Some(supported);
    }
  }
  // If server offers something newer or compatible with our latest, use latest
  if server_version >= LATEST_PROTOCOL_VERSION {
    Some(LATEST_PROTOCOL_VERSION)
  } else {
    None
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_protocol_negotiation() {
    assert_eq!(negotiate_protocol_version("2024-11-05"), Some("2024-11-05"));
    assert_eq!(negotiate_protocol_version("2024-10-07"), Some("2024-10-07"));
    assert_eq!(negotiate_protocol_version("2025-01-01"), Some("2024-11-05"));
    assert_eq!(negotiate_protocol_version("2023-01-01"), None);
  }

  #[test]
  fn test_call_tool_result_text_extraction() {
    let res = CallToolResult {
      content: vec![
        McpContent {
          content_type: "text".into(),
          text: Some("hello".into()),
          data: None,
          mime_type: None,
        },
        McpContent {
          content_type: "text".into(),
          text: Some("world".into()),
          data: None,
          mime_type: None,
        },
      ],
      is_error: None,
    };
    assert_eq!(res.text_content(), "hello\nworld");
  }
}
