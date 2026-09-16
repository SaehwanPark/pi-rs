//! MCP transport abstractions and stdio implementation.

use std::{
  collections::{BTreeMap, HashMap},
  fmt,
  io::{BufReader, Read, Write},
  process::{Child, ChildStdin, Command, Stdio},
  sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc::{self, SyncSender},
  },
  time::Duration,
};

use pi_rs_core::{BoundedLineReader, LineOverflow, ToolExecutionContext};
use serde_json::Value;

use crate::{
  error::McpError,
  protocol::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse},
};

/// MCP transport contract.
pub trait McpTransport: Send + Sync {
  /// Send a JSON-RPC request and wait for the response.
  fn call(&self, method: &str, params: Option<Value>) -> Result<Value, McpError>;

  /// Send a request with cancellation/deadline context. Legacy transports may
  /// delegate to [`Self::call`], while process-backed transports can interrupt
  /// their worker and classify the result honestly.
  fn call_with_context(
    &self,
    method: &str,
    params: Option<Value>,
    _context: &ToolExecutionContext,
  ) -> Result<Value, McpError> {
    self.call(method, params)
  }

  /// Send a JSON-RPC notification (no response expected).
  fn notify(&self, method: &str, params: Option<Value>) -> Result<(), McpError>;

  /// Whether the transport channel is still active and connected.
  fn is_alive(&self) -> bool;

  /// Record the negotiated MCP protocol version for transports that put it on the wire.
  fn set_protocol_version(&self, _version: &str) {}

  /// Terminate the transport.
  fn close(&mut self) -> Result<(), McpError>;
}

/// Default timeout for waiting on an MCP request response.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Bound one network response so a broken server cannot make a call allocate without limit.
const MAX_HTTP_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_HTTP_ERROR_BYTES: u64 = 8 * 1024;
const MAX_HTTP_REQUEST_BYTES: usize = 8 * 1024 * 1024;
const MAX_STDIO_RESPONSE_LINE_BYTES: usize = 1024 * 1024;
const MAX_STDIO_DIAGNOSTIC_LINE_BYTES: usize = 64 * 1024;
const RESPONSE_POLL_INTERVAL: Duration = Duration::from_millis(50);

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
    #[cfg(not(target_os = "windows"))]
    {
      use std::os::unix::process::CommandExt;
      cmd.process_group(0);
    }

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
          let mut reader = BufReader::new(stdout);
          let mut lines = BoundedLineReader::new();
          loop {
            let line = match lines.read_line(
              &mut reader,
              MAX_STDIO_RESPONSE_LINE_BYTES,
              LineOverflow::Reject,
            ) {
              Ok(Some(line)) => line,
              Ok(None) => break,
              Err(error) => {
                alive_clone.store(false, Ordering::SeqCst);
                let mut map = pending_clone.lock().unwrap();
                for (_, sender) in map.drain() {
                  let _ = sender.send(Err(McpError::Protocol(format!(
                    "bounded MCP stdout read failed: {error}"
                  ))));
                }
                return;
              }
            };
            let trimmed = String::from_utf8_lossy(line.as_bytes());
            let trimmed = trimmed.trim();
            if trimmed.is_empty() {
              continue;
            }

            if let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(trimmed) {
              if let Some(id_u64) = resp.id.as_u64() {
                let mut map = pending_clone.lock().unwrap();
                if let Some(sender) = map.remove(&id_u64) {
                  let outcome = match validate_jsonrpc_response(&resp) {
                    Err(message) => Err(McpError::Protocol(message)),
                    Ok(()) => {
                      if let Some(err) = resp.error {
                        Err(McpError::JsonRpc {
                          code: err.code,
                          message: err.message,
                          data: err.data,
                        })
                      } else {
                        Ok(resp.result.unwrap_or(Value::Null))
                      }
                    }
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
          let mut reader = BufReader::new(stderr);
          let mut lines = BoundedLineReader::new();
          while let Ok(Some(line)) = lines.read_line(
            &mut reader,
            MAX_STDIO_DIAGNOSTIC_LINE_BYTES,
            LineOverflow::Truncate,
          ) {
            let mut text = String::from_utf8_lossy(line.as_bytes())
              .trim_end_matches(['\n', '\r'])
              .to_owned();
            if line.is_truncated() {
              text.push_str(" [line truncated]");
            }
            let mut log = stderr_log_clone.lock().unwrap();
            if log.len() < 100 {
              log.push(text);
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

  fn terminate_child(&self) {
    self.alive.store(false, Ordering::SeqCst);
    let Ok(mut child_guard) = self.child.lock() else {
      return;
    };
    if let Some(mut child) = child_guard.take() {
      terminate_process_tree(&mut child);
    }
  }
}

impl McpTransport for StdioTransport {
  fn call(&self, method: &str, params: Option<Value>) -> Result<Value, McpError> {
    self.call_with_context(method, params, &ToolExecutionContext::unbounded())
  }

  fn call_with_context(
    &self,
    method: &str,
    params: Option<Value>,
    context: &ToolExecutionContext,
  ) -> Result<Value, McpError> {
    if !self.is_alive() {
      return Err(McpError::ProcessExited(None));
    }
    if context.is_cancelled_or_expired() {
      self.terminate_child();
      return Err(if context.is_cancelled() {
        McpError::Cancelled
      } else {
        McpError::Timeout
      });
    }

    let id = self.next_id.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = mpsc::sync_channel(1);

    {
      let mut map = self.pending.lock().unwrap();
      map.insert(id, tx);
    }

    let req = JsonRpcRequest::new(id, method, params);
    let serialized = match serde_json::to_string(&req) {
      Ok(serialized) => serialized,
      Err(error) => {
        self.pending.lock().unwrap().remove(&id);
        return Err(McpError::Protocol(format!(
          "failed to serialize request: {error}"
        )));
      }
    };

    {
      let mut stdin = self.stdin.lock().unwrap();
      if let Err(error) = writeln!(stdin, "{serialized}").and_then(|_| stdin.flush()) {
        self.pending.lock().unwrap().remove(&id);
        self.alive.store(false, Ordering::SeqCst);
        self.terminate_child();
        return Err(McpError::Transport(format!(
          "failed to write to child stdin: {error}"
        )));
      }
    }

    let call_deadline = std::time::Instant::now() + self.timeout;
    loop {
      if context.is_cancelled_or_expired() {
        self.pending.lock().unwrap().remove(&id);
        self.terminate_child();
        return Err(if context.is_cancelled() {
          McpError::Cancelled
        } else {
          McpError::Timeout
        });
      }
      let remaining = call_deadline.saturating_duration_since(std::time::Instant::now());
      if remaining.is_zero() {
        self.pending.lock().unwrap().remove(&id);
        self.terminate_child();
        return Err(McpError::Timeout);
      }
      let wait = context
        .remaining()
        .map(|deadline| deadline.min(remaining).min(RESPONSE_POLL_INTERVAL))
        .unwrap_or_else(|| remaining.min(RESPONSE_POLL_INTERVAL));
      match rx.recv_timeout(wait) {
        Ok(res) => return res,
        Err(mpsc::RecvTimeoutError::Timeout) => continue,
        Err(mpsc::RecvTimeoutError::Disconnected) => {
          self.alive.store(false, Ordering::SeqCst);
          return Err(McpError::ProcessExited(None));
        }
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
    self.terminate_child();
    Ok(())
  }
}

impl Drop for StdioTransport {
  fn drop(&mut self) {
    let _ = self.close();
  }
}

/// Streamable HTTP MCP transport.
///
/// The initial network contract is deliberately bounded: each JSON-RPC request is a
/// POST, responses may be `application/json` or a single JSON-RPC message carried in
/// an SSE `data:` field, and the server may establish an `Mcp-Session-Id` header.
/// Long-lived server push and resumable event streams remain outside this transport;
/// MCP tool discovery and calls are still lazy because construction performs no I/O.
pub struct HttpTransport {
  agent: ureq::Agent,
  url: String,
  headers: BTreeMap<String, String>,
  next_id: AtomicU64,
  session_id: Mutex<Option<String>>,
  protocol_version: Mutex<Option<String>>,
  alive: AtomicBool,
}

impl fmt::Debug for HttpTransport {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    let header_names: Vec<&str> = self.headers.keys().map(String::as_str).collect();
    formatter
      .debug_struct("HttpTransport")
      .field("url", &redact_url(&self.url))
      .field("header_names", &header_names)
      .field("alive", &self.is_alive())
      .finish()
  }
}

impl HttpTransport {
  /// Build a transport without connecting to the endpoint.
  pub fn new(url: impl Into<String>, headers: BTreeMap<String, String>) -> Result<Self, McpError> {
    let url = url.into();
    if url.trim().is_empty() {
      return Err(McpError::Transport(
        "MCP network URL must not be empty".into(),
      ));
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
      return Err(McpError::Transport(
        "MCP network URL must start with http:// or https://".into(),
      ));
    }
    if url_has_userinfo(&url) {
      return Err(McpError::Transport(
        "MCP network URL must not contain userinfo credentials".into(),
      ));
    }
    if headers.iter().any(|(name, value)| {
      !valid_header_name(name)
        || value.contains('\r')
        || value.contains('\n')
        || is_reserved_header(name)
    }) {
      return Err(McpError::Transport(
        "MCP network headers contain an invalid or reserved field".into(),
      ));
    }
    let agent = ureq::builder()
      .timeout_connect(DEFAULT_REQUEST_TIMEOUT)
      .timeout_read(DEFAULT_REQUEST_TIMEOUT)
      .timeout_write(DEFAULT_REQUEST_TIMEOUT)
      .try_proxy_from_env(true)
      .build();
    Ok(Self {
      agent,
      url,
      headers,
      next_id: AtomicU64::new(1),
      session_id: Mutex::new(None),
      protocol_version: Mutex::new(None),
      alive: AtomicBool::new(true),
    })
  }

  fn send(&self, body: &str) -> Result<ureq::Response, McpError> {
    if body.len() > MAX_HTTP_REQUEST_BYTES {
      return Err(McpError::Protocol(format!(
        "MCP HTTP request exceeds {MAX_HTTP_REQUEST_BYTES} bytes"
      )));
    }
    if !self.is_alive() {
      return Err(McpError::Transport("MCP HTTP transport is closed".into()));
    }
    let mut request = self.agent.post(&self.url);
    for (name, value) in &self.headers {
      request = request.set(name, value);
    }
    request = request
      .set("content-type", "application/json")
      .set("accept", "application/json, text/event-stream");
    if let Some(session_id) = self.session_id.lock().unwrap().as_deref() {
      request = request.set("Mcp-Session-Id", session_id);
    }
    if let Some(version) = self.protocol_version.lock().unwrap().as_deref() {
      request = request.set("MCP-Protocol-Version", version);
    }
    match request.send_string(body) {
      Ok(response) => {
        if let Some(session_id) = response.header("Mcp-Session-Id") {
          *self.session_id.lock().unwrap() = Some(session_id.to_string());
        }
        Ok(response)
      }
      Err(ureq::Error::Status(status, response)) => {
        let body = read_http_body_with_limit(response, MAX_HTTP_ERROR_BYTES).unwrap_or_default();
        let diagnostic = String::from_utf8_lossy(&body);
        Err(McpError::Transport(format!(
          "MCP HTTP status {status}: {}",
          diagnostic.chars().take(1024).collect::<String>()
        )))
      }
      Err(error) => {
        self.alive.store(false, Ordering::SeqCst);
        Err(McpError::Transport(format!(
          "MCP HTTP request failed: {error}"
        )))
      }
    }
  }
}

impl McpTransport for HttpTransport {
  fn call(&self, method: &str, params: Option<Value>) -> Result<Value, McpError> {
    let id = self.next_id.fetch_add(1, Ordering::SeqCst);
    let request = JsonRpcRequest::new(id, method, params);
    let serialized = serde_json::to_string(&request)
      .map_err(|error| McpError::Protocol(format!("failed to serialize request: {error}")))?;
    let response = self.send(&serialized)?;
    decode_http_response(response, id)
  }

  fn notify(&self, method: &str, params: Option<Value>) -> Result<(), McpError> {
    let notification = JsonRpcNotification::new(method, params);
    let serialized = serde_json::to_string(&notification)
      .map_err(|error| McpError::Protocol(format!("failed to serialize notification: {error}")))?;
    let response = self.send(&serialized)?;
    // A streamable HTTP server may answer a notification with 202 and no body. Consume
    // and bound any body so the pooled connection can be reused without parsing a result.
    let _ = read_http_body(response)?;
    Ok(())
  }

  fn is_alive(&self) -> bool {
    self.alive.load(Ordering::SeqCst)
  }

  fn set_protocol_version(&self, version: &str) {
    *self.protocol_version.lock().unwrap() = Some(version.to_string());
  }

  fn close(&mut self) -> Result<(), McpError> {
    self.alive.store(false, Ordering::SeqCst);
    Ok(())
  }
}

fn terminate_process_tree(child: &mut Child) {
  #[cfg(unix)]
  {
    let pid = child.id().to_string();
    let _ = Command::new("kill")
      .args(["-KILL", &format!("-{pid}")])
      .stderr(Stdio::null())
      .status();
  }
  #[cfg(windows)]
  {
    let pid = child.id().to_string();
    let _ = Command::new("taskkill")
      .args(["/PID", pid.as_str(), "/T", "/F"])
      .status();
  }
  let _ = child.kill();
  let _ = child.wait();
}

fn valid_header_name(name: &str) -> bool {
  !name.is_empty()
    && name.bytes().all(|byte| {
      byte.is_ascii_alphanumeric()
        || matches!(
          byte,
          b'!'
            | b'#'
            | b'$'
            | b'%'
            | b'&'
            | b'\''
            | b'*'
            | b'+'
            | b'-'
            | b'.'
            | b'^'
            | b'_'
            | b'`'
            | b'|'
            | b'~'
        )
    })
}

fn is_reserved_header(name: &str) -> bool {
  [
    "accept",
    "content-type",
    "content-length",
    "host",
    "mcp-session-id",
    "mcp-protocol-version",
  ]
  .iter()
  .any(|reserved| name.eq_ignore_ascii_case(reserved))
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

fn url_has_userinfo(url: &str) -> bool {
  let Some((_, authority_and_path)) = url.split_once("://") else {
    return false;
  };
  authority_and_path
    .split_once('/')
    .map(|(authority, _)| authority.contains('@'))
    .unwrap_or_else(|| authority_and_path.contains('@'))
}

fn decode_http_response(response: ureq::Response, expected_id: u64) -> Result<Value, McpError> {
  let content_type = response
    .header("content-type")
    .unwrap_or("")
    .to_ascii_lowercase();
  let body = read_http_body(response)?;
  let value = if content_type.contains("text/event-stream") {
    parse_sse_json(&body, expected_id)?
  } else {
    serde_json::from_slice::<Value>(&body)
      .map_err(|error| McpError::Protocol(format!("invalid MCP HTTP JSON response: {error}")))?
  };
  let response: JsonRpcResponse = serde_json::from_value(value)
    .map_err(|error| McpError::Protocol(format!("invalid MCP JSON-RPC response: {error}")))?;
  validate_jsonrpc_response(&response).map_err(McpError::Protocol)?;
  if response.id.as_u64() != Some(expected_id) {
    return Err(McpError::Protocol(format!(
      "MCP JSON-RPC response id {:?} does not match request id {expected_id}",
      response.id
    )));
  }
  if let Some(error) = response.error {
    return Err(McpError::JsonRpc {
      code: error.code,
      message: error.message,
      data: error.data,
    });
  }
  Ok(response.result.unwrap_or(Value::Null))
}

fn validate_jsonrpc_response(response: &JsonRpcResponse) -> Result<(), String> {
  if response.jsonrpc != "2.0" {
    return Err(format!(
      "MCP JSON-RPC response has unsupported version {:?}",
      response.jsonrpc
    ));
  }
  if response.result.is_some() == response.error.is_some() {
    return Err("MCP JSON-RPC response must contain exactly one of result or error".into());
  }
  Ok(())
}

fn parse_sse_json(body: &[u8], expected_id: u64) -> Result<Value, McpError> {
  let text = std::str::from_utf8(body)
    .map_err(|error| McpError::Protocol(format!("MCP SSE response is not UTF-8: {error}")))?;
  for line in text.lines() {
    let Some(data) = line.strip_prefix("data:") else {
      continue;
    };
    let data = data.trim();
    if data.is_empty() || data == "[DONE]" {
      continue;
    }
    let value: Value = match serde_json::from_str(data) {
      Ok(value) => value,
      Err(_) => continue,
    };
    if value.get("id").and_then(Value::as_u64) == Some(expected_id) {
      return Ok(value);
    }
  }
  Err(McpError::Protocol(
    "MCP SSE response contained no data message".into(),
  ))
}

fn read_http_body(response: ureq::Response) -> Result<Vec<u8>, McpError> {
  read_http_body_with_limit(response, MAX_HTTP_RESPONSE_BYTES)
}

fn read_http_body_with_limit(response: ureq::Response, limit: u64) -> Result<Vec<u8>, McpError> {
  let mut body = Vec::new();
  let mut reader = response.into_reader().take(limit + 1);
  reader
    .read_to_end(&mut body)
    .map_err(|error| McpError::Transport(format!("cannot read MCP HTTP response: {error}")))?;
  if body.len() as u64 > limit {
    return Err(McpError::Protocol(format!(
      "MCP HTTP response exceeds {limit} bytes"
    )));
  }
  Ok(body)
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
  use std::{net::TcpListener, sync::mpsc, thread};

  fn http_fixture(
    responses: Vec<String>,
  ) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
    let address = listener.local_addr().expect("fixture address");
    let (seen_tx, seen_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
      for response in responses {
        let (mut stream, _) = listener.accept().expect("accept fixture request");
        stream
          .set_read_timeout(Some(Duration::from_secs(2)))
          .expect("set fixture timeout");
        let mut request = Vec::new();
        loop {
          let mut byte = [0u8; 1];
          stream.read_exact(&mut byte).expect("read fixture headers");
          request.push(byte[0]);
          if request.ends_with(b"\r\n\r\n") {
            break;
          }
        }
        let header_text = String::from_utf8_lossy(&request).to_string();
        let content_length = header_text
          .lines()
          .find_map(|line| {
            line
              .strip_prefix("Content-Length:")
              .or_else(|| line.strip_prefix("content-length:"))
          })
          .and_then(|value| value.trim().parse::<usize>().ok())
          .unwrap_or(0);
        let mut body = vec![0u8; content_length];
        stream.read_exact(&mut body).expect("read fixture body");
        let mut seen = header_text;
        seen.push_str(&String::from_utf8_lossy(&body));
        seen_tx.send(seen).expect("send fixture request");
        let response = if !response.to_ascii_lowercase().contains("connection:") {
          response.replacen("\r\n\r\n", "\r\nConnection: close\r\n\r\n", 1)
        } else {
          response
        };
        stream
          .write_all(response.as_bytes())
          .expect("write fixture response");
        stream.flush().expect("flush fixture response");
        let _ = stream.shutdown(std::net::Shutdown::Both);
      }
    });
    (format!("http://{address}/mcp"), seen_rx, handle)
  }

  #[test]
  fn stdio_response_overflow_closes_the_protocol_before_materializing_the_line() {
    let script = "IFS= read -r line; head -c 1048577 /dev/zero";
    let transport = StdioTransport::spawn(
      "sh",
      &["-c".to_string(), script.to_string()],
      &BTreeMap::new(),
    )
    .expect("spawns overflow fixture");
    let error = transport.call("overflow", None).unwrap_err();
    assert!(format!("{error}").contains("bounded MCP stdout"), "{error}");
    assert!(!transport.is_alive());
  }

  #[test]
  fn stdio_call_cancellation_terminates_a_hung_server() {
    let transport = StdioTransport::spawn(
      "sh",
      &[
        "-c".to_string(),
        "while IFS= read -r line; do sleep 30; done".to_string(),
      ],
      &BTreeMap::new(),
    )
    .expect("spawns hanging fixture");
    let cancel = pi_rs_core::CancelToken::new();
    let trigger = cancel.clone();
    let killer = thread::spawn(move || {
      thread::sleep(Duration::from_millis(100));
      trigger.cancel();
    });
    let context = ToolExecutionContext::new(cancel, Duration::from_secs(10));
    let result = transport.call_with_context("hang", None, &context);
    killer.join().unwrap();
    assert!(matches!(result, Err(McpError::Cancelled)), "{result:?}");
    assert!(!transport.is_alive());
  }

  #[test]
  fn http_transport_round_trips_json_and_propagates_session_id() {
    let responses = vec![
      concat!(
        "HTTP/1.1 200 OK\r\n",
        "Content-Type: application/json\r\n",
        "Connection: close\r\n",
        "Mcp-Session-Id: session-1\r\n",
        "Content-Length: 45\r\n\r\n",
        r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#
      )
      .to_string(),
      concat!(
        "HTTP/1.1 200 OK\r\n",
        "Content-Type: text/event-stream\r\n",
        "Connection: close\r\n",
        "Content-Length: 53\r\n\r\n",
        "data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"ok\":true}}\n\n"
      )
      .to_string(),
    ];
    let (url, seen, handle) = http_fixture(responses);
    let transport = HttpTransport::new(url, BTreeMap::new()).expect("transport");
    assert_eq!(transport.call("first", None).unwrap()["ok"], true);
    assert_eq!(transport.call("second", None).unwrap()["ok"], true);
    let first = seen.recv().unwrap();
    let second = seen.recv().unwrap();
    assert!(first.contains("\"method\":\"first\""), "{first}");
    assert!(second.contains("Mcp-Session-Id: session-1"), "{second}");
    assert!(second.contains("\"method\":\"second\""), "{second}");
    handle.join().unwrap();
  }

  #[test]
  fn http_transport_sends_negotiated_header_and_redacts_urls() {
    let body = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
    let response = format!(
      "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
      body.len()
    );
    let (url, seen, handle) = http_fixture(vec![response]);
    let transport =
      HttpTransport::new(format!("{url}?token=secret-value"), BTreeMap::new()).expect("transport");
    transport.set_protocol_version("2024-11-05");
    transport
      .call("tools/list", None)
      .expect("request succeeds");
    let request = seen.recv().unwrap();
    assert!(
      request.contains("MCP-Protocol-Version: 2024-11-05"),
      "{request}"
    );
    let debug = format!("{transport:?}");
    assert!(
      !debug.contains("secret-value"),
      "debug output leaked URL query: {debug}"
    );
    handle.join().unwrap();

    let error = HttpTransport::new(
      "http://127.0.0.1:1/mcp",
      BTreeMap::from([("MCP-Protocol-Version".into(), "spoofed".into())]),
    )
    .unwrap_err();
    assert!(format!("{error}").contains("headers"));
  }

  #[test]
  fn http_transport_rejects_invalid_jsonrpc_and_oversized_requests() {
    let body = r#"{"jsonrpc":"1.0","id":1,"result":{"ok":true}}"#;
    let response = format!(
      "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
      body.len()
    );
    let (url, _seen, handle) = http_fixture(vec![response]);
    let transport = HttpTransport::new(url, BTreeMap::new()).expect("transport");
    let error = transport.call("invalid-version", None).unwrap_err();
    assert!(format!("{error}").contains("unsupported version"));
    handle.join().unwrap();

    let transport =
      HttpTransport::new("http://127.0.0.1:1/mcp", BTreeMap::new()).expect("transport");
    let oversized = json!({"value": "x".repeat(MAX_HTTP_REQUEST_BYTES)});
    let error = transport.call("oversized", Some(oversized)).unwrap_err();
    assert!(format!("{error}").contains("request exceeds"));
  }

  #[test]
  fn http_transport_bounds_response_and_rejects_invalid_headers() {
    let body = "x".repeat((MAX_HTTP_RESPONSE_BYTES + 1) as usize);
    let response = format!(
      "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
      body.len(),
      body
    );
    let (url, _seen, handle) = http_fixture(vec![response]);
    let transport = HttpTransport::new(url, BTreeMap::new()).expect("transport");
    let error = transport.call("large", None).unwrap_err();
    assert!(format!("{error}").contains("exceeds"));
    handle.join().unwrap();

    let error = HttpTransport::new(
      "http://127.0.0.1:1/mcp",
      BTreeMap::from([("x-test\nname".into(), "x".into())]),
    )
    .unwrap_err();
    assert!(format!("{error}").contains("headers"));
  }

  #[test]
  fn http_transport_rejects_wrong_id_and_keeps_http_status_recoverable() {
    let body = r#"{"jsonrpc":"2.0","id":99,"result":{"ok":true}}"#;
    let wrong_id_response = format!(
      "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
      body.len()
    );
    let (url, _seen, handle) = http_fixture(vec![wrong_id_response]);
    let transport = HttpTransport::new(url, BTreeMap::new()).expect("transport");
    let error = transport.call("wrong-id", None).unwrap_err();
    assert!(format!("{error}").contains("does not match"));
    assert!(transport.is_alive());
    handle.join().unwrap();

    let body = "x".repeat(16 * 1024);
    let status_response = format!(
      "HTTP/1.1 429 Too Many Requests\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}",
      body.len()
    );
    let (url, _seen, handle) = http_fixture(vec![status_response]);
    let transport = HttpTransport::new(url, BTreeMap::new()).expect("transport");
    let error = transport.call("status", None).unwrap_err();
    let rendered = format!("{error}");
    assert!(rendered.contains("HTTP status 429"));
    assert!(rendered.len() < 2_000, "status diagnostics must be bounded");
    assert!(transport.is_alive());
    handle.join().unwrap();
  }

  #[test]
  fn http_transport_accepts_empty_notification_response() {
    let response = "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n".to_string();
    let (url, _seen, handle) = http_fixture(vec![response]);
    let transport = HttpTransport::new(url, BTreeMap::new()).expect("transport");
    transport
      .notify("notifications/initialized", None)
      .expect("empty notification response succeeds");
    handle.join().unwrap();
  }

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
