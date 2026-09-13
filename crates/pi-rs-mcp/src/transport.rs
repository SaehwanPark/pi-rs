//! MCP transport abstractions and stdio implementation.

use std::{
  collections::{BTreeMap, HashMap},
  io::{BufRead, BufReader, Write},
  process::{Child, ChildStdin, Command, Stdio},
  sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, SyncSender},
  },
  time::Duration,
};

use serde_json::Value;

use crate::{
  error::McpError,
  protocol::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse},
};

/// MCP transport contract.
pub trait McpTransport: Send + Sync {
  /// Send a JSON-RPC request and wait for the response.
  fn call(&self, method: &str, params: Option<Value>) -> Result<Value, McpError>;

  /// Send a JSON-RPC notification (no response expected).
  fn notify(&self, method: &str, params: Option<Value>) -> Result<(), McpError>;

  /// Whether the transport channel is still active and connected.
  fn is_alive(&self) -> bool;

  /// Terminate the transport.
  fn close(&mut self) -> Result<(), McpError>;
}

/// Default timeout for waiting on an MCP request response.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

type PendingResponseSender = SyncSender<Result<Value, McpError>>;
type PendingRequests = Arc<Mutex<HashMap<u64, PendingResponseSender>>>;

/// Stdio-based MCP transport connecting to an external server process.
pub struct StdioTransport {
  stdin: Arc<Mutex<ChildStdin>>,
  pending: PendingRequests,
  next_id: AtomicU64,
  alive: Arc<AtomicBool>,
  timeout: Duration,
  child: Arc<Mutex<Option<Child>>>,
  stderr_log: Arc<Mutex<Vec<String>>>,
}

impl StdioTransport {
  /// Spawn an external process and connect over stdio pipes.
  pub fn spawn(
    command: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
  ) -> Result<Self, McpError> {
    let mut cmd = Command::new(command);
    cmd.args(args);
    for (k, v) in env {
      cmd.env(k, v);
    }
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd
      .spawn()
      .map_err(|e| McpError::Transport(format!("failed to spawn '{command}': {e}")))?;

    let stdin = child
      .stdin
      .take()
      .ok_or_else(|| McpError::Transport("failed to capture child stdin".into()))?;
    let stdout = child
      .stdout
      .take()
      .ok_or_else(|| McpError::Transport("failed to capture child stdout".into()))?;
    let stderr = child
      .stderr
      .take()
      .ok_or_else(|| McpError::Transport("failed to capture child stderr".into()))?;

    let pending: PendingRequests = Arc::new(Mutex::new(HashMap::new()));
    let alive = Arc::new(AtomicBool::new(true));
    let stderr_log = Arc::new(Mutex::new(Vec::new()));

    // Background thread to read stdout JSON-RPC messages.
    {
      let pending_clone = Arc::clone(&pending);
      let alive_clone = Arc::clone(&alive);
      std::thread::Builder::new()
        .name("mcp-stdout-reader".into())
        .spawn(move || {
          let reader = BufReader::new(stdout);
          for line in reader.lines() {
            let line = match line {
              Ok(l) => l,
              Err(_) => break,
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
              continue;
            }

            if let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(trimmed) {
              if let Some(id_u64) = resp.id.as_u64() {
                let mut map = pending_clone.lock().unwrap();
                if let Some(sender) = map.remove(&id_u64) {
                  let outcome = if let Some(err) = resp.error {
                    Err(McpError::JsonRpc {
                      code: err.code,
                      message: err.message,
                      data: err.data,
                    })
                  } else {
                    Ok(resp.result.unwrap_or(Value::Null))
                  };
                  let _ = sender.send(outcome);
                }
              }
            }
          }
          alive_clone.store(false, Ordering::SeqCst);
          // Drain any remaining pending requests with a ProcessExited error
          let mut map = pending_clone.lock().unwrap();
          for (_, sender) in map.drain() {
            let _ = sender.send(Err(McpError::ProcessExited(None)));
          }
        })
        .map_err(|e| McpError::Transport(format!("failed to spawn reader thread: {e}")))?;
    }

    // Background thread to drain stderr so the child process does not block on a full pipe.
    {
      let stderr_log_clone = Arc::clone(&stderr_log);
      std::thread::Builder::new()
        .name("mcp-stderr-reader".into())
        .spawn(move || {
          let reader = BufReader::new(stderr);
          for line in reader.lines() {
            if let Ok(l) = line {
              let mut log = stderr_log_clone.lock().unwrap();
              if log.len() < 100 {
                log.push(l);
              }
            } else {
              break;
            }
          }
        })
        .map_err(|e| McpError::Transport(format!("failed to spawn stderr reader thread: {e}")))?;
    }

    Ok(Self {
      stdin: Arc::new(Mutex::new(stdin)),
      pending,
      next_id: AtomicU64::new(1),
      alive,
      timeout: DEFAULT_REQUEST_TIMEOUT,
      child: Arc::new(Mutex::new(Some(child))),
      stderr_log,
    })
  }

  pub fn with_timeout(mut self, timeout: Duration) -> Self {
    self.timeout = timeout;
    self
  }

  /// Recent stderr output captured from the child process.
  pub fn stderr_lines(&self) -> Vec<String> {
    self.stderr_log.lock().unwrap().clone()
  }
}

impl McpTransport for StdioTransport {
  fn call(&self, method: &str, params: Option<Value>) -> Result<Value, McpError> {
    if !self.is_alive() {
      return Err(McpError::ProcessExited(None));
    }

    let id = self.next_id.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = mpsc::sync_channel(1);

    {
      let mut map = self.pending.lock().unwrap();
      map.insert(id, tx);
    }

    let req = JsonRpcRequest::new(id, method, params);
    let serialized = serde_json::to_string(&req)
      .map_err(|e| McpError::Protocol(format!("failed to serialize request: {e}")))?;

    {
      let mut stdin = self.stdin.lock().unwrap();
      writeln!(stdin, "{serialized}").map_err(|e| {
        self.alive.store(false, Ordering::SeqCst);
        McpError::Transport(format!("failed to write to child stdin: {e}"))
      })?;
      stdin.flush().map_err(|e| {
        self.alive.store(false, Ordering::SeqCst);
        McpError::Transport(format!("failed to flush child stdin: {e}"))
      })?;
    }

    match rx.recv_timeout(self.timeout) {
      Ok(res) => res,
      Err(mpsc::RecvTimeoutError::Timeout) => {
        let mut map = self.pending.lock().unwrap();
        map.remove(&id);
        Err(McpError::Timeout)
      }
      Err(mpsc::RecvTimeoutError::Disconnected) => {
        self.alive.store(false, Ordering::SeqCst);
        Err(McpError::ProcessExited(None))
      }
    }
  }

  fn notify(&self, method: &str, params: Option<Value>) -> Result<(), McpError> {
    if !self.is_alive() {
      return Err(McpError::ProcessExited(None));
    }

    let notification = JsonRpcNotification::new(method, params);
    let serialized = serde_json::to_string(&notification)
      .map_err(|e| McpError::Protocol(format!("failed to serialize notification: {e}")))?;

    let mut stdin = self.stdin.lock().unwrap();
    writeln!(stdin, "{serialized}").map_err(|e| {
      self.alive.store(false, Ordering::SeqCst);
      McpError::Transport(format!("failed to write notification to stdin: {e}"))
    })?;
    stdin.flush().map_err(|e| {
      self.alive.store(false, Ordering::SeqCst);
      McpError::Transport(format!("failed to flush notification to stdin: {e}"))
    })?;
    Ok(())
  }

  fn is_alive(&self) -> bool {
    self.alive.load(Ordering::SeqCst)
  }

  fn close(&mut self) -> Result<(), McpError> {
    self.alive.store(false, Ordering::SeqCst);
    let mut child_guard = self.child.lock().unwrap();
    if let Some(mut child) = child_guard.take() {
      let _ = child.kill();
      let _ = child.wait();
    }
    Ok(())
  }
}

impl Drop for StdioTransport {
  fn drop(&mut self) {
    let _ = self.close();
  }
}

/// A scriptable mock transport for deterministic testing without external processes.
#[derive(Default)]
pub struct MockTransport {
  responses: Mutex<HashMap<String, Value>>,
  calls: Mutex<Vec<(String, Option<Value>)>>,
  notifications: Mutex<Vec<(String, Option<Value>)>>,
  alive: AtomicBool,
}

impl MockTransport {
  pub fn new() -> Self {
    Self {
      responses: Mutex::new(HashMap::new()),
      calls: Mutex::new(Vec::new()),
      notifications: Mutex::new(Vec::new()),
      alive: AtomicBool::new(true),
    }
  }

  pub fn on(&self, method: impl Into<String>, response: Value) {
    self
      .responses
      .lock()
      .unwrap()
      .insert(method.into(), response);
  }

  pub fn recorded_calls(&self) -> Vec<(String, Option<Value>)> {
    self.calls.lock().unwrap().clone()
  }

  pub fn recorded_notifications(&self) -> Vec<(String, Option<Value>)> {
    self.notifications.lock().unwrap().clone()
  }
}

impl McpTransport for MockTransport {
  fn call(&self, method: &str, params: Option<Value>) -> Result<Value, McpError> {
    if !self.is_alive() {
      return Err(McpError::Transport("mock transport is closed".into()));
    }
    self
      .calls
      .lock()
      .unwrap()
      .push((method.to_string(), params));
    let map = self.responses.lock().unwrap();
    map
      .get(method)
      .cloned()
      .ok_or_else(|| McpError::Protocol(format!("mock transport has no response for '{method}'")))
  }

  fn notify(&self, method: &str, params: Option<Value>) -> Result<(), McpError> {
    if !self.is_alive() {
      return Err(McpError::Transport("mock transport is closed".into()));
    }
    self
      .notifications
      .lock()
      .unwrap()
      .push((method.to_string(), params));
    Ok(())
  }

  fn is_alive(&self) -> bool {
    self.alive.load(Ordering::SeqCst)
  }

  fn close(&mut self) -> Result<(), McpError> {
    self.alive.store(false, Ordering::SeqCst);
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn test_mock_transport_call_and_notify() {
    let mock = MockTransport::new();
    mock.on("test/method", json!({"status": "ok"}));

    let res = mock
      .call("test/method", Some(json!({"arg": 1})))
      .expect("call succeeds");
    assert_eq!(res["status"], "ok");

    mock
      .notify("test/notification", Some(json!({"event": 123})))
      .expect("notify succeeds");

    let calls = mock.recorded_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "test/method");

    let notifs = mock.recorded_notifications();
    assert_eq!(notifs.len(), 1);
    assert_eq!(notifs[0].0, "test/notification");
  }
}
