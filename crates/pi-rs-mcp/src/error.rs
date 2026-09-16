//! MCP error types.

use std::fmt;

/// Errors arising from MCP protocol, transport, or tool execution.
#[derive(Debug)]
pub enum McpError {
  /// I/O or pipe error in the transport layer.
  Transport(String),
  /// Protocol violation (invalid JSON-RPC, missing fields, version mismatch).
  Protocol(String),
  /// JSON-RPC error returned by the server.
  JsonRpc {
    code: i64,
    message: String,
    data: Option<serde_json::Value>,
  },
  /// The requested MCP server configuration was not found.
  ServerNotFound(String),
  /// The child process exited prematurely.
  ProcessExited(Option<i32>),
  /// Execution timed out.
  Timeout,
  /// Execution was cancelled by the caller.
  Cancelled,
  /// MCP tool call failed.
  ToolExecution(String),
  /// Protocol version negotiation failed.
  NegotiationFailed(String),
}

impl fmt::Display for McpError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Transport(msg) => write!(f, "MCP transport error: {msg}"),
      Self::Protocol(msg) => write!(f, "MCP protocol error: {msg}"),
      Self::JsonRpc {
        code,
        message,
        data,
      } => {
        write!(f, "MCP JSON-RPC error ({code}): {message}")?;
        if let Some(d) = data {
          write!(f, " data: {d}")?;
        }
        Ok(())
      }
      Self::ServerNotFound(name) => write!(f, "MCP server '{name}' not found"),
      Self::ProcessExited(code) => match code {
        Some(c) => write!(f, "MCP process exited with code {c}"),
        None => write!(f, "MCP process exited unexpectedly"),
      },
      Self::Timeout => write!(f, "MCP operation timed out"),
      Self::Cancelled => write!(f, "MCP operation was cancelled"),
      Self::ToolExecution(msg) => write!(f, "MCP tool execution error: {msg}"),
      Self::NegotiationFailed(msg) => write!(f, "MCP negotiation failed: {msg}"),
    }
  }
}

impl std::error::Error for McpError {}
