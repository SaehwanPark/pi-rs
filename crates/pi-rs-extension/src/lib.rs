//! Lazy, line-oriented Node host for the selected Pi extension API.
//!
//! Constructing [`ExtensionHost`] only stores configuration. Node is launched by
//! [`ExtensionHost::start`] (or the first tool/command dispatch), and extension
//! failures are kept at this boundary rather than becoming core session errors.

#![forbid(unsafe_code)]

use std::{
  collections::BTreeMap,
  io::{BufRead, BufReader, Write},
  path::{Path, PathBuf},
  process::{Child, ChildStdin, ChildStdout, Command, Stdio},
  sync::{Arc, Mutex},
};

use pi_rs_core::{Tool, ToolError, ToolMetadata, ToolOutcome, ToolProgress, ToolRequest};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const BOOTSTRAP: &str = include_str!("bootstrap.mjs");

/// Configuration for the compatibility host. Paths are trusted inputs supplied
/// by the caller; the host never discovers or executes project files implicitly.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionHostConfig {
  pub modules: Vec<PathBuf>,
  pub node_binary: PathBuf,
  pub working_directory: Option<PathBuf>,
}

impl ExtensionHostConfig {
  pub fn new(modules: impl IntoIterator<Item = impl Into<PathBuf>>) -> Self {
    Self {
      modules: modules.into_iter().map(Into::into).collect(),
      ..Self::default()
    }
  }

  pub fn with_node_binary(mut self, binary: impl Into<PathBuf>) -> Self {
    self.node_binary = binary.into();
    self
  }

  pub fn with_working_directory(mut self, directory: impl Into<PathBuf>) -> Self {
    self.working_directory = Some(directory.into());
    self
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostStatus {
  NotStarted,
  Starting,
  Ready,
  Stopped,
  Failed,
}

/// Lifecycle names intentionally supported by the compatibility boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleEvent {
  SessionStart,
  SessionShutdown,
  TurnStart,
  TurnEnd,
  ToolCall,
  ToolResult,
  Context,
}

impl LifecycleEvent {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::SessionStart => "session_start",
      Self::SessionShutdown => "session_shutdown",
      Self::TurnStart => "turn_start",
      Self::TurnEnd => "turn_end",
      Self::ToolCall => "tool_call",
      Self::ToolResult => "tool_result",
      Self::Context => "context",
    }
  }
}

/// Failures at the process/protocol boundary. [`HostError::ProcessLost`] is
/// intentionally distinct from an extension throwing an ordinary error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail")]
pub enum HostError {
  InvalidModule(String),
  Spawn(String),
  Io(String),
  Protocol(String),
  Extension {
    message: String,
    code: Option<String>,
  },
  ProcessLost(String),
}

impl std::fmt::Display for HostError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::InvalidModule(s) => write!(f, "invalid extension module: {s}"),
      Self::Spawn(s) => write!(f, "extension host could not start: {s}"),
      Self::Io(s) => write!(f, "extension host I/O failed: {s}"),
      Self::Protocol(s) => write!(f, "extension host protocol failed: {s}"),
      Self::Extension { message, code } => match code {
        Some(c) => write!(f, "extension error [{c}]: {message}"),
        None => write!(f, "extension error: {message}"),
      },
      Self::ProcessLost(s) => write!(f, "extension host process lost: {s}"),
    }
  }
}
impl std::error::Error for HostError {}

pub type ExtensionHostError = HostError;

pub mod host {
  pub use super::{
    DispatchResult, ExtensionCommandInfo, ExtensionHost, ExtensionHostConfig, ExtensionHostError,
    ExtensionToolInfo, HostError, HostStatus, LifecycleEvent, UiEvent,
  };
}

pub mod protocol {
  pub use super::{RpcError, RpcRequest, RpcResponse};
}

pub mod tool {
  pub use super::{ExtensionTool, ExtensionToolInfo};
}

/// A registered Pi extension tool as reported by Node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionToolInfo {
  pub name: String,
  pub label: Option<String>,
  pub description: String,
  pub parameters: Value,
  pub read_only: bool,
  pub idempotent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionCommandInfo {
  pub name: String,
  pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiEvent {
  pub kind: String,
  pub key: Option<String>,
  pub text: Option<String>,
  pub value: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct DispatchResult {
  pub text: Option<String>,
  pub value: Option<Value>,
  #[serde(default)]
  pub ui: Vec<UiEvent>,
  #[serde(default)]
  pub tools: Vec<ExtensionToolInfo>,
  #[serde(default)]
  pub commands: Vec<ExtensionCommandInfo>,
}

#[derive(Clone)]
pub struct ExtensionHost {
  inner: Arc<Mutex<HostInner>>,
}

struct HostInner {
  config: ExtensionHostConfig,
  status: HostStatus,
  child: Option<Process>,
  tools: BTreeMap<String, ExtensionToolInfo>,
  commands: BTreeMap<String, ExtensionCommandInfo>,
}
struct Process {
  child: Child,
  stdin: ChildStdin,
  stdout: BufReader<ChildStdout>,
  next_id: u64,
}

impl Drop for Process {
  fn drop(&mut self) {
    let _ = self.child.kill();
    let _ = self.child.wait();
  }
}

impl ExtensionHost {
  pub fn new(config: ExtensionHostConfig) -> Self {
    Self {
      inner: Arc::new(Mutex::new(HostInner {
        config,
        status: HostStatus::NotStarted,
        child: None,
        tools: BTreeMap::new(),
        commands: BTreeMap::new(),
      })),
    }
  }

  pub fn status(&self) -> HostStatus {
    self.inner.lock().expect("extension host lock").status
  }
  pub fn config(&self) -> ExtensionHostConfig {
    self
      .inner
      .lock()
      .expect("extension host lock")
      .config
      .clone()
  }
  pub fn tools(&self) -> Vec<ExtensionToolInfo> {
    self
      .inner
      .lock()
      .expect("extension host lock")
      .tools
      .values()
      .cloned()
      .collect()
  }
  pub fn commands(&self) -> Vec<ExtensionCommandInfo> {
    self
      .inner
      .lock()
      .expect("extension host lock")
      .commands
      .values()
      .cloned()
      .collect()
  }

  /// Start and load all configured modules. With no modules this is a cheap
  /// no-op and, importantly, does not launch Node.
  pub fn start(&self) -> Result<(), HostError> {
    let mut h = self.inner.lock().expect("extension host lock");
    h.start()
  }

  /// Stop the Node process without starting a host that has not been activated.
  pub fn stop(&self) {
    let mut h = self.inner.lock().expect("extension host lock");
    h.stop();
  }

  /// Dispatch `session_shutdown` when active, then always stop the process.
  pub fn shutdown(&self, value: Value) -> Result<DispatchResult, HostError> {
    let active = self.status() == HostStatus::Ready && !self.config().modules.is_empty();
    let result = if active {
      self.dispatch_event(LifecycleEvent::SessionShutdown, value)
    } else {
      Ok(DispatchResult::default())
    };
    self.stop();
    result
  }

  pub fn dispatch_tool(&self, name: &str, arguments: Value) -> Result<DispatchResult, HostError> {
    self.dispatch_tool_call(name, "", arguments)
  }

  pub fn dispatch_tool_call(
    &self,
    name: &str,
    call_id: &str,
    arguments: Value,
  ) -> Result<DispatchResult, HostError> {
    self.request(
      "call_tool",
      json!({"name": name, "tool_call_id": call_id, "arguments": arguments}),
    )
  }
  pub fn dispatch_command(
    &self,
    name: &str,
    arguments: Value,
  ) -> Result<DispatchResult, HostError> {
    self.request(
      "call_command",
      json!({"name": name, "arguments": arguments}),
    )
  }
  pub fn dispatch_lifecycle(&self, event: &str, value: Value) -> Result<DispatchResult, HostError> {
    if !matches!(
      event,
      "session_start"
        | "session_shutdown"
        | "turn_start"
        | "turn_end"
        | "tool_call"
        | "tool_result"
        | "context"
    ) {
      return Err(HostError::Protocol(format!(
        "unsupported lifecycle event '{event}'"
      )));
    }
    self.request("lifecycle", json!({"event": event, "value": value}))
  }

  pub fn dispatch_event(
    &self,
    event: LifecycleEvent,
    value: Value,
  ) -> Result<DispatchResult, HostError> {
    self.dispatch_lifecycle(event.as_str(), value)
  }
  pub fn dispatch_context(&self, value: Value) -> Result<DispatchResult, HostError> {
    self.request("context", value)
  }

  fn request(&self, method: &str, params: Value) -> Result<DispatchResult, HostError> {
    let mut h = self.inner.lock().expect("extension host lock");
    if h.config.modules.is_empty() {
      return Err(HostError::Extension {
        message: "no extensions configured".into(),
        code: None,
      });
    }
    h.start()?;
    h.request(method, params)
  }

  /// Build wrappers for all tools currently registered by the extension.
  /// Calling this method starts the host because registration is provided by JS.
  pub fn tool_wrappers(&self) -> Result<Vec<ExtensionTool>, HostError> {
    self.start()?;
    Ok(
      self
        .tools()
        .into_iter()
        .map(|info| ExtensionTool {
          host: self.clone(),
          info,
        })
        .collect(),
    )
  }
}

impl HostInner {
  fn stop(&mut self) {
    self.child = None;
    self.status = HostStatus::Stopped;
  }

  fn start(&mut self) -> Result<(), HostError> {
    if self.status == HostStatus::Ready {
      return Ok(());
    }
    if self.config.modules.is_empty() {
      self.status = HostStatus::Ready;
      return Ok(());
    }
    for module in &self.config.modules {
      if !matches!(
        module.extension().and_then(|x| x.to_str()),
        Some("ts" | "js" | "mjs" | "cjs")
      ) {
        self.status = HostStatus::Failed;
        return Err(HostError::InvalidModule(module.display().to_string()));
      }
      if let Err(error) = resolve_module_path(module, self.config.working_directory.as_deref()) {
        self.status = HostStatus::Failed;
        return Err(error);
      }
    }
    self.status = HostStatus::Starting;
    let binary = if self.config.node_binary.as_os_str().is_empty() {
      Path::new("node").to_path_buf()
    } else {
      self.config.node_binary.clone()
    };
    let mut command = Command::new(binary);
    if self
      .config
      .modules
      .iter()
      .any(|p| p.extension().is_some_and(|e| e == "ts"))
    {
      command.arg("--experimental-strip-types");
    }
    let child = command
      .arg("--input-type=module")
      .arg("--eval")
      .arg(BOOTSTRAP)
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::inherit());
    if let Some(dir) = &self.config.working_directory {
      child.current_dir(dir);
    }
    let mut child = child.spawn().map_err(|e| {
      self.status = HostStatus::Failed;
      HostError::Spawn(e.to_string())
    })?;
    let stdin = child
      .stdin
      .take()
      .ok_or_else(|| HostError::Spawn("stdin unavailable".into()))?;
    let stdout = child
      .stdout
      .take()
      .ok_or_else(|| HostError::Spawn("stdout unavailable".into()))?;
    self.child = Some(Process {
      child,
      stdin,
      stdout: BufReader::new(stdout),
      next_id: 1,
    });
    let modules: Vec<String> = self
      .config
      .modules
      .iter()
      .map(|path| path_to_file_url(path, self.config.working_directory.as_deref()))
      .collect::<Result<_, _>>()
      .inspect_err(|_| {
        self.status = HostStatus::Failed;
        self.child = None;
      })?;
    let response = self.request("load", json!({"modules": modules}));
    match response {
      Ok(result) => {
        self.tools = result
          .tools
          .into_iter()
          .map(|tool| (tool.name.clone(), tool))
          .collect();
        self.commands = result
          .commands
          .into_iter()
          .map(|command| (command.name.clone(), command))
          .collect();
        self.status = HostStatus::Ready;
        Ok(())
      }
      Err(error) => {
        self.status = HostStatus::Failed;
        self.child = None;
        Err(error)
      }
    }
  }

  fn request(&mut self, method: &str, params: Value) -> Result<DispatchResult, HostError> {
    let process = self
      .child
      .as_mut()
      .ok_or_else(|| HostError::ProcessLost("host is not running".into()))?;
    if let Some(status) = process
      .child
      .try_wait()
      .map_err(|e| HostError::Io(e.to_string()))?
    {
      self.child = None;
      self.status = HostStatus::Stopped;
      return Err(HostError::ProcessLost(format!("exited with {status}")));
    }
    let id = process.next_id;
    process.next_id += 1;
    let request = json!({"id": id, "method": method, "params": params});
    serde_json::to_writer(&mut process.stdin, &request)
      .map_err(|e| HostError::Io(e.to_string()))?;
    let write_result = process
      .stdin
      .write_all(b"\n")
      .and_then(|_| process.stdin.flush());
    if let Err(error) = write_result {
      self.child = None;
      self.status = HostStatus::Stopped;
      return Err(HostError::ProcessLost(error.to_string()));
    }
    let mut line = String::new();
    let read_result = process.stdout.read_line(&mut line);
    if let Err(error) = read_result {
      self.child = None;
      self.status = HostStatus::Stopped;
      return Err(HostError::ProcessLost(error.to_string()));
    }
    if line.is_empty() {
      self.child = None;
      self.status = HostStatus::Stopped;
      return Err(HostError::ProcessLost("EOF from Node host".into()));
    }
    let response: RpcResponse = serde_json::from_str(&line).map_err(|e| {
      self.status = HostStatus::Failed;
      HostError::Protocol(format!("invalid response: {e}"))
    })?;
    if response.id != id {
      return Err(HostError::Protocol(format!(
        "response id {} does not match {id}",
        response.id
      )));
    }
    if !response.ok {
      let error = response.error.unwrap_or(RpcError {
        message: "unknown extension failure".into(),
        code: None,
      });
      return Err(HostError::Extension {
        message: error.message,
        code: error.code,
      });
    }
    serde_json::from_value(response.result.unwrap_or_default())
      .map_err(|e| HostError::Protocol(format!("invalid result: {e}")))
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
  pub id: u64,
  pub method: String,
  pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
  pub id: u64,
  pub ok: bool,
  #[serde(default)]
  pub result: Option<Value>,
  #[serde(default)]
  pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
  pub message: String,
  pub code: Option<String>,
}

/// A [`Tool`] backed by a registered JavaScript extension tool.
pub struct ExtensionTool {
  host: ExtensionHost,
  info: ExtensionToolInfo,
}
impl ExtensionTool {
  pub fn info(&self) -> &ExtensionToolInfo {
    &self.info
  }
}
impl Tool for ExtensionTool {
  fn metadata(&self) -> ToolMetadata {
    if self.info.read_only {
      ToolMetadata::read_only(&self.info.name, &self.info.description)
    } else {
      ToolMetadata::mutating(
        &self.info.name,
        &self.info.description,
        self.info.idempotent,
      )
    }
  }
  fn arguments_schema(&self) -> Value {
    self.info.parameters.clone()
  }
  fn execute(
    &self,
    request: &ToolRequest,
    progress: &mut dyn ToolProgress,
  ) -> Result<ToolOutcome, ToolError> {
    let result = self.host.dispatch_tool_call(
      &self.info.name,
      &request.call_id.to_string(),
      request.arguments.clone(),
    );
    match result {
      Ok(result) => {
        if let Some(text) = result.text.clone() {
          if !text.is_empty() {
            progress.emit(&pi_rs_core::ToolChunk::new(&text));
          }
        }
        Ok(ToolOutcome::succeeded(result.text.unwrap_or_else(|| {
          result.value.map_or_else(String::new, |v| v.to_string())
        })))
      }
      Err(HostError::Extension { message, code }) => {
        let message = match code {
          Some(code) => format!("{message} ({code})"),
          None => message,
        };
        if self.info.read_only {
          Ok(ToolOutcome::failed(message))
        } else {
          Ok(ToolOutcome::unknown(format!(
            "extension tool completion was not observed: {message}"
          )))
        }
      }
      Err(error) => Err(ToolError::after_start(error.to_string())),
    }
  }
}

fn resolve_module_path(
  path: &Path,
  working_directory: Option<&Path>,
) -> Result<PathBuf, HostError> {
  let absolute = if path.is_absolute() {
    path.to_path_buf()
  } else {
    let base = match working_directory {
      Some(directory) => directory.to_path_buf(),
      None => std::env::current_dir().map_err(|e| HostError::InvalidModule(e.to_string()))?,
    };
    base.join(path)
  };
  let canonical = std::fs::canonicalize(&absolute)
    .map_err(|error| HostError::InvalidModule(format!("{}: {error}", absolute.display())))?;
  if !canonical.is_file() {
    return Err(HostError::InvalidModule(format!(
      "{} is not a file",
      absolute.display()
    )));
  }
  Ok(canonical)
}

fn path_to_file_url(path: &Path, working_directory: Option<&Path>) -> Result<String, HostError> {
  let canonical = resolve_module_path(path, working_directory)?;
  let text = canonical
    .to_str()
    .ok_or_else(|| HostError::InvalidModule("module path is not UTF-8".into()))?;
  // Windows canonical paths may use backslashes or the extended-length `\\?\\` prefix.
  // Normalize those forms before escaping so Node receives an absolute file URL rather
  // than a URL with an invalid drive/host component.
  #[cfg(windows)]
  let (prefix, text) = {
    let mut text = text.replace('\\', "/");
    if let Some(rest) = text.strip_prefix("//?/UNC/") {
      text = format!("//{rest}");
    } else if let Some(rest) = text.strip_prefix("//?/") {
      text = rest.to_string();
    }
    if text.starts_with("//") {
      ("file:", text)
    } else {
      ("file://", format!("/{text}"))
    }
  };
  #[cfg(not(windows))]
  let (prefix, text) = ("file://", text.to_owned());

  // Encode every byte that is not safe in a file URL. In particular, '?' must
  // not become a query separator and '#' must not become a fragment.
  let mut escaped = String::with_capacity(text.len());
  for byte in text.bytes() {
    if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/' | b':') {
      escaped.push(byte as char);
    } else {
      escaped.push('%');
      escaped.push(char::from(b"0123456789ABCDEF"[(byte >> 4) as usize]));
      escaped.push(char::from(b"0123456789ABCDEF"[(byte & 0x0F) as usize]));
    }
  }
  Ok(format!("{prefix}{escaped}"))
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn construction_is_lazy_and_empty_start_does_not_spawn() {
    let host = ExtensionHost::new(ExtensionHostConfig::default());
    assert_eq!(host.status(), HostStatus::NotStarted);
    host.start().unwrap();
    assert_eq!(host.status(), HostStatus::Ready);
  }
  #[test]
  fn module_validation_is_before_spawn() {
    let host = ExtensionHost::new(ExtensionHostConfig::new(["missing.txt"]));
    assert!(matches!(host.start(), Err(HostError::InvalidModule(_))));
    assert_eq!(host.status(), HostStatus::Failed);
  }

  #[test]
  fn file_urls_are_absolute_on_all_platforms() {
    let module = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join("../../tests/compat/extensions/phase8-fixture.ts");
    let url = path_to_file_url(&module, None).unwrap();
    assert!(url.starts_with("file:///"), "{url}");
    assert!(!url.contains('\\'), "{url}");
  }

  #[test]
  fn pi_style_fixture_registers_and_dispatches_selected_surface() {
    if std::process::Command::new("node")
      .arg("--version")
      .output()
      .is_err()
    {
      return;
    }
    let module = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
      .join("../../tests/compat/extensions/phase8-fixture.ts");
    let host = ExtensionHost::new(ExtensionHostConfig::new([module]));
    host.start().unwrap();
    assert_eq!(host.tools()[0].name, "fixture-greet");
    let result = host
      .dispatch_tool("fixture-greet", json!({"name": "Ada"}))
      .unwrap();
    assert_eq!(result.text.as_deref(), Some("hello Ada"));
    let command = host
      .dispatch_command("fixture-command", json!("go"))
      .unwrap();
    assert!(
      command
        .ui
        .iter()
        .any(|event| event.kind == "notify" && event.text.as_deref() == Some("command:go"))
    );
    let lifecycle = host.dispatch_lifecycle("session_start", json!({})).unwrap();
    assert_eq!(lifecycle.ui.len(), 3);
    let context = host.dispatch_context(json!({"messages": []})).unwrap();
    assert_eq!(
      context.value.unwrap()["messages"][0]["content"],
      "extension context"
    );
  }
}
