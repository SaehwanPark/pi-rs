//! The built-in tool set.
//!
//! One module per tool, plus the registry that knows the set, the policy, and
//! the lifecycle. Each tool is a value implementing [`pi_rs_core::tool::Tool`],
//! which keeps them independently testable and lets an extension replace one
//! without touching the others.

pub mod edit;
pub mod exec;
pub mod grep;
pub mod paths;
pub mod read;
pub mod reduce;
pub mod registry;
pub mod write;

pub use edit::EditTool;
pub use exec::ExecTool;
pub use grep::GrepTool;
pub use paths::{PathError, Workspace};
pub use read::ReadTool;
pub use reduce::Reduction;
pub use registry::{Approval, ApprovalGate, AutoApprove, DenyAll, Executed, ToolRegistry};
pub use write::WriteTool;

use std::time::Instant;

use pi_rs_core::{ToolError, ToolOutcome, ToolProgress, ToolRequest};

/// Time budget for one local file operation.
pub(crate) const WRITE_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

/// The shared plumbing a built-in tool needs.
///
/// `Rc<RefCell<..>>` rather than shared ownership of a `dyn`: tools are
/// `Send + Sync`, so the state they hold must be atomically shared, and the two
/// pieces of state they actually need (the workspace root and a deadline) are
/// cheap to clone and never mutated.
#[derive(Clone)]
pub struct Runtime {
  pub(crate) workspace: Workspace,
  pub(crate) max_output_bytes: u64,
  /// Shell budget, in milliseconds. Public so `exec` can read it without a
  /// second accessor layer.
  pub(crate) shell_timeout_ms: u64,
  pub(crate) max_line_bytes: usize,
}

impl Runtime {
  /// Cap on bytes captured from a command before the result is marked truncated.
  ///
  /// Deliberately larger than the model-visible budget: the tool keeps more than
  /// it shows so the reduction has real head and tail to work with, and the
  /// capture ceiling is what stops a runaway process from consuming memory.
  pub(crate) fn exec_capture_limit(&self) -> usize {
    usize::try_from(self.max_output_bytes)
      .unwrap_or(usize::MAX)
      .saturating_mul(8)
  }

  pub(crate) fn new(workspace: Workspace) -> Self {
    Self {
      workspace,
      max_output_bytes: 8 * 1024,
      shell_timeout_ms: 120_000,
      max_line_bytes: 4 * 1024,
    }
  }

  /// Return the complete result to the registry.
  ///
  /// The registry is the reduction boundary because it owns the `Executed`
  /// record and can preserve the original bytes for durable recovery before
  /// handing the bounded form to the runtime.
  pub(crate) fn finish(&self, text: String) -> ToolOutcome {
    ToolOutcome::succeeded(text)
  }

  pub(crate) fn with_policy(mut self, policy: &pi_rs_core::ToolPolicy) -> Self {
    self.max_output_bytes = policy.max_output_bytes;
    self.shell_timeout_ms = policy.shell_timeout_ms;
    if let Some(cwd) = policy.cwd.as_deref() {
      if let Ok(workspace) = Workspace::new(cwd) {
        self.workspace = workspace;
      }
    }
    self
  }
}

/// Read an argument as a string, or explain the failure in model-facing terms.
pub(crate) fn arg_str<'a>(request: &'a ToolRequest, name: &str) -> Result<&'a str, ToolError> {
  request
    .arguments
    .get(name)
    .and_then(|value| value.as_str())
    .ok_or_else(|| {
      ToolError::new(format!(
        "{}: missing required string argument '{name}'",
        request.name
      ))
    })
}

/// Optional boolean argument with a default.
pub(crate) fn arg_bool(request: &ToolRequest, name: &str, default: bool) -> bool {
  request
    .arguments
    .get(name)
    .and_then(|value| value.as_bool())
    .unwrap_or(default)
}

/// Optional unsigned integer argument.
pub(crate) fn arg_u64(request: &ToolRequest, name: &str) -> Option<u64> {
  request.arguments.get(name).and_then(|value| value.as_u64())
}

/// A deadline shared by every stage of one execution.
///
/// Cancellation is checked at the boundaries where continuing would be wrong:
/// before starting, and between chunks while streaming.
#[derive(Debug, Clone)]
pub struct Deadline {
  started: Instant,
  pub(crate) limit: std::time::Duration,
}

impl Deadline {
  pub fn new(limit: std::time::Duration) -> Self {
    Self {
      started: Instant::now(),
      limit,
    }
  }

  pub fn elapsed(&self) -> std::time::Duration {
    self.started.elapsed()
  }

  pub fn remaining(&self) -> std::time::Duration {
    self.limit.saturating_sub(self.elapsed())
  }

  pub fn expired(&self) -> bool {
    self.elapsed() >= self.limit
  }

  pub fn elapsed_ms(&self) -> u64 {
    self.elapsed().as_millis() as u64
  }
}

/// A progress sink that also enforces a byte budget and a deadline.
///
/// Tools stream through it, so a runaway `find /` or a compile log cannot fill
/// memory or outrun its timeout just because the tool itself kept reading.
pub(crate) struct BoundedProgress<'a> {
  inner: &'a mut dyn ToolProgress,
  budget: usize,
  pub(crate) streamed: usize,
  pub(crate) truncated: bool,
  deadline: Deadline,
  pub(crate) timed_out: bool,
}

impl<'a> BoundedProgress<'a> {
  /// Wrap a sink with a byte budget and a wall-clock deadline.
  ///
  /// Every tool streams through this, so the deadline is passed in rather than
  /// defaulted: the tool knows whether it is a 60s search or a 2s write.
  pub(crate) fn new(inner: &'a mut dyn ToolProgress, budget: usize, deadline: Deadline) -> Self {
    Self {
      inner,
      budget,
      streamed: 0,
      truncated: false,
      deadline,
      timed_out: false,
    }
  }

  /// Forward text, reporting whether the caller should stop.
  ///
  /// Returns `false` once the budget or the deadline is spent. The caller must
  /// honor that rather than keep producing text.
  pub(crate) fn send(&mut self, text: &str) -> bool {
    if text.is_empty() {
      return true;
    }
    if self.deadline.expired() {
      self.timed_out = true;
      return false;
    }
    let remaining = self.budget.saturating_sub(self.streamed);
    if remaining == 0 {
      self.truncated = true;
      return false;
    }
    let slice = if text.len() <= remaining {
      text
    } else {
      self.truncated = true;
      floor(text, remaining)
    };
    self.streamed += slice.len();
    self.inner.emit(&pi_rs_core::ToolChunk::new(slice));
    !self.truncated
  }
}

fn floor(text: &str, bytes: usize) -> &str {
  if bytes >= text.len() {
    return text;
  }
  let mut cursor = bytes;
  while cursor > 0 && !text.is_char_boundary(cursor) {
    cursor -= 1;
  }
  &text[..cursor]
}

/// The shared test harness for tool modules.
#[cfg(test)]
pub(crate) mod testutil {
  use std::cell::RefCell;

  use pi_rs_core::{ToolChunk, ToolProgress};

  use crate::{Runtime, paths::Workspace};

  /// A recorded stream of progress chunks.
  #[derive(Default)]
  pub(crate) struct Recorder {
    pub(crate) chunks: RefCell<String>,
  }

  impl Recorder {
    pub(crate) fn text(&self) -> String {
      self.chunks.borrow().clone()
    }
  }

  impl ToolProgress for Recorder {
    fn emit(&mut self, chunk: &ToolChunk) {
      self.chunks.borrow_mut().push_str(&chunk.text);
    }
  }

  pub(crate) fn runtime(dir: &tempfile::TempDir) -> Runtime {
    Runtime::new(Workspace::new(dir.path()).unwrap())
  }

  #[test]
  fn extreme_output_limit_saturates_exec_capture_arithmetic() {
    let dir = tempfile::tempdir().unwrap();
    let mut runtime = runtime(&dir);
    runtime.max_output_bytes = u64::MAX;
    assert_eq!(runtime.exec_capture_limit(), usize::MAX);
  }
}
