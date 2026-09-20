//! MCP server manager managing configurations, lifecycle, and lazy discovery.

use std::{
  collections::BTreeMap,
  sync::Arc,
  time::{Duration, Instant},
};

use rupi_core::McpServerConfig;

use crate::{
  client::McpClient,
  error::McpError,
  tool::McpTool,
  transport::{HttpTransport, McpTransport, StdioTransport},
};

/// Status report for an MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerStatus {
  pub name: String,
  pub command: String,
  pub enabled: bool,
  pub active: bool,
  pub tool_count: usize,
  pub first_use_latency_ms: Option<u64>,
}

/// Manager responsible for lazy activation, tool discovery, and lifecycle of MCP servers.
pub struct McpManager {
  configs: BTreeMap<String, McpServerConfig>,
  clients: BTreeMap<String, Arc<McpClient>>,
  active_tools: BTreeMap<String, Vec<McpTool>>,
  first_use_latencies: BTreeMap<String, Duration>,
}

fn redact_url(url: &str) -> String {
  if let Some((base, _)) = url.split_once('?') {
    return format!("{base}?[redacted]");
  }
  if let Some((base, _)) = url.split_once('#') {
    return format!("{base}#[redacted]");
  }
  url.to_string()
}

impl McpManager {
  /// Create a new manager with the provided server configurations.
  ///
  /// INVARIANT: No processes are spawned and no connections are established
  /// during manager construction. Activation is strictly lazy.
  pub fn new(configs: Vec<McpServerConfig>) -> Self {
    let mut map = BTreeMap::new();
    for cfg in configs {
      map.insert(cfg.name.clone(), cfg);
    }
    Self {
      configs: map,
      clients: BTreeMap::new(),
      active_tools: BTreeMap::new(),
      first_use_latencies: BTreeMap::new(),
    }
  }

  /// Whether the named server is currently connected and active.
  pub fn is_active(&self, name: &str) -> bool {
    self
      .clients
      .get(name)
      .map(|c| c.is_alive())
      .unwrap_or(false)
  }

  /// List all configured server names.
  pub fn server_names(&self) -> Vec<String> {
    self.configs.keys().cloned().collect()
  }

  /// Get status of all configured servers.
  pub fn statuses(&self) -> Vec<McpServerStatus> {
    self
      .configs
      .values()
      .map(|cfg| {
        let active = self.is_active(&cfg.name);
        let tool_count = self
          .active_tools
          .get(&cfg.name)
          .map(|t| t.len())
          .unwrap_or(0);
        let first_use_latency_ms = self
          .first_use_latencies
          .get(&cfg.name)
          .map(|d| d.as_millis() as u64);
        McpServerStatus {
          name: cfg.name.clone(),
          command: cfg
            .url
            .as_deref()
            .map(redact_url)
            .unwrap_or_else(|| cfg.command.clone()),
          enabled: cfg.enabled,
          active,
          tool_count,
          first_use_latency_ms,
        }
      })
      .collect()
  }

  /// Enable and connect a server on demand, discovering its tools.
  ///
  /// Returns the newly normalized tools.
  pub fn enable_server(&mut self, name: &str) -> Result<Vec<McpTool>, McpError> {
    if let Some(tools) = self.active_tools.get(name) {
      if self.is_active(name) {
        return Ok(tools.clone());
      }
    }

    let config = self
      .configs
      .get(name)
      .cloned()
      .ok_or_else(|| McpError::ServerNotFound(name.to_string()))?;

    let start = Instant::now();

    let transport: Arc<dyn McpTransport> = if let Some(url) = &config.url {
      Arc::new(HttpTransport::new(url.clone(), config.headers.clone())?)
    } else {
      Arc::new(StdioTransport::spawn(
        &config.command,
        &config.args,
        &config.env,
      )?)
    };
    let client = Arc::new(McpClient::new(transport));

    // Perform handshake and negotiate protocol
    client.initialize()?;

    // Discover tools exposed by the server
    let discovered = client.list_tools()?;
    let elapsed = start.elapsed();
    self.first_use_latencies.insert(name.to_string(), elapsed);

    let mut mcp_tools = Vec::new();
    for def in discovered {
      let is_read_only = config.read_only_tools.contains(&def.name);
      let tool = McpTool::new(name, def, Arc::clone(&client), is_read_only);
      mcp_tools.push(tool);
    }

    self.clients.insert(name.to_string(), client);
    self
      .active_tools
      .insert(name.to_string(), mcp_tools.clone());

    Ok(mcp_tools)
  }

  /// Connect a server with a custom transport (used for testing).
  pub fn enable_server_with_transport(
    &mut self,
    name: &str,
    transport: Arc<dyn McpTransport>,
  ) -> Result<Vec<McpTool>, McpError> {
    let config = self
      .configs
      .get(name)
      .cloned()
      .ok_or_else(|| McpError::ServerNotFound(name.to_string()))?;

    let start = Instant::now();
    let client = Arc::new(McpClient::new(transport));
    client.initialize()?;
    let discovered = client.list_tools()?;
    self
      .first_use_latencies
      .insert(name.to_string(), start.elapsed());

    let mut mcp_tools = Vec::new();
    for def in discovered {
      let is_read_only = config.read_only_tools.contains(&def.name);
      let tool = McpTool::new(name, def, Arc::clone(&client), is_read_only);
      mcp_tools.push(tool);
    }

    self.clients.insert(name.to_string(), client);
    self
      .active_tools
      .insert(name.to_string(), mcp_tools.clone());

    Ok(mcp_tools)
  }

  /// Disable and disconnect a server, removing its tools.
  pub fn disable_server(&mut self, name: &str) -> Result<(), McpError> {
    self.clients.remove(name);
    self.active_tools.remove(name);
    Ok(())
  }

  /// All currently active tools across all enabled servers.
  pub fn all_active_tools(&self) -> Vec<McpTool> {
    let mut out = Vec::new();
    for tools in self.active_tools.values() {
      out.extend(tools.clone());
    }
    out
  }

  /// First-use latency measured for a server.
  pub fn first_use_latency(&self, name: &str) -> Option<Duration> {
    self.first_use_latencies.get(name).copied()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::transport::MockTransport;
  use rupi_core::Tool;
  use serde_json::json;
  use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
  };

  #[test]
  fn test_manager_selects_lazy_http_transport() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind HTTP fixture");
    let address = listener.local_addr().expect("HTTP fixture address");
    let server = thread::spawn(move || {
      for _ in 0..3 {
        let (mut stream, _) = listener.accept().expect("accept HTTP request");
        let mut request = Vec::new();
        loop {
          let mut byte = [0u8; 1];
          stream.read_exact(&mut byte).expect("read HTTP headers");
          request.push(byte[0]);
          if request.ends_with(b"\r\n\r\n") {
            break;
          }
        }
        let header_text = String::from_utf8_lossy(&request);
        let content_length = header_text
          .lines()
          .find_map(|line| {
            line
              .strip_prefix("Content-Length:")
              .or_else(|| line.strip_prefix("content-length:"))
          })
          .and_then(|value| value.trim().parse::<usize>().ok())
          .unwrap_or(0);
        let mut body = vec![0; content_length];
        stream.read_exact(&mut body).expect("read HTTP body");
        let body = String::from_utf8_lossy(&body);
        let (status, response) = if body.contains("initialize") {
          (
            "200 OK",
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"http-fixture","version":"1"}}}"#,
          )
        } else if body.contains("notifications/initialized") {
          ("202 Accepted", "")
        } else {
          (
            "200 OK",
            r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"search","description":"search","inputSchema":{"type":"object"}}]}}"#,
          )
        };
        let response_text = format!(
          "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{response}",
          response.len()
        );
        stream
          .write_all(response_text.as_bytes())
          .expect("write HTTP response");
        stream.flush().expect("flush HTTP response");
        let _ = stream.shutdown(std::net::Shutdown::Both);
      }
    });

    let url = format!("http://{address}/mcp");
    let config = McpServerConfig::new("remote", "").with_url(url.clone());
    let mut manager = McpManager::new(vec![config]);
    let tools = manager
      .enable_server("remote")
      .expect("HTTP activation succeeds");
    assert_eq!(tools.len(), 1);
    assert_eq!(manager.statuses()[0].command, url);
    server.join().unwrap();
  }

  #[test]
  fn test_manager_lazy_discovery() {
    let cfg = McpServerConfig::new("rkb", "rkb-server").with_args(vec!["--stdio".into()]);
    let mut manager = McpManager::new(vec![cfg]);

    // Invariant: inert before activation
    assert!(!manager.is_active("rkb"));
    assert_eq!(manager.all_active_tools().len(), 0);

    let mock = Arc::new(MockTransport::new());
    mock.on(
      "initialize",
      json!({
        "protocolVersion": "2024-11-05",
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "rkb-server", "version": "0.1.0" }
      }),
    );
    mock.on(
      "tools/list",
      json!({
        "tools": [
          {
            "name": "search",
            "description": "search knowledge base",
            "inputSchema": { "type": "object" }
          }
        ]
      }),
    );

    let tools = manager
      .enable_server_with_transport("rkb", mock)
      .expect("activation succeeds");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].metadata().name, "mcp__rkb__search");
    assert!(manager.is_active("rkb"));
    assert!(manager.first_use_latency("rkb").is_some());

    // Disabling deactivates cleanly
    manager.disable_server("rkb").expect("disable succeeds");
    assert!(!manager.is_active("rkb"));
    assert_eq!(manager.all_active_tools().len(), 0);
  }
}
