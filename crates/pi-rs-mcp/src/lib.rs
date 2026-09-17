//! `pi-rs-mcp` — MCP (Model Context Protocol) client for `pi-rs`.
//!
//! This crate implements the MCP client subsystem for `pi-rs`, providing:
//! - JSON-RPC 2.0 stdio transport connecting to child MCP processes.
//! - Protocol version negotiation and capability handshake.
//! - Lazy server activation: servers are configured but not connected at startup,
//!   protecting the cold and warm startup latency budgets.
//! - Tool normalization into the standard [`pi_rs_core::tool::Tool`] abstraction.
//! - First-class lifecycle events and honest provenance preservation.
//! - An explicit [`worker`] server boundary for coarse agent operations and resources.

#![forbid(unsafe_code)]

pub mod client;
pub mod error;
pub mod manager;
pub mod protocol;
mod relay;
pub mod tool;
pub mod transport;
pub mod worker;

pub use client::McpClient;
pub use error::McpError;
pub use manager::{McpManager, McpServerStatus};
pub use pi_rs_core::McpServerConfig;
pub use protocol::{
  CallToolParams, CallToolResult, InitializeResult, ListToolsResult, McpContent, McpToolDefinition,
  ServerCapabilities, ServerInfo,
};
pub use tool::{McpTool, mcp_namespaced_tool_name};
pub use transport::{
  DEFAULT_REQUEST_TIMEOUT, HttpTransport, McpTransport, MockTransport, StdioTransport,
};
pub use worker::{
  BranchRequest, CancelRequest, CancelResult, CompactRequest, CompactResult, ContinueRequest,
  StartRequest, WorkerArtifact, WorkerCancelToken, WorkerCheckpoint, WorkerCompactMode,
  WorkerCompactRequest, WorkerDiff, WorkerDiffFile, WorkerEngine, WorkerError, WorkerExecution,
  WorkerExternalContext, WorkerFailover, WorkerMcpServer, WorkerMessage, WorkerModelEpoch,
  WorkerResourceContent, WorkerResourceDescription, WorkerResourceListResult,
  WorkerResourceReadResult, WorkerRunHandle, WorkerRunRequest, WorkerServerError, WorkerService,
  WorkerSnapshot, WorkerState, WorkerStatus, WorkerSummary, WorkerSummarySource, WorkerTraceEntry,
  serve_stdio,
};
