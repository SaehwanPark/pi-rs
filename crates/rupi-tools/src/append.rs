//! `append` — append bytes to a file as an explicitly non-idempotent action.
//!
//! Appending is intentionally separate from `write`: the same request can
//! validly add its bytes twice, so a suffix match after a crash cannot prove
//! that this invocation performed the append. Unknown append calls therefore
//! remain manual-inspection barriers.

use std::{fs, io::Write, time::Duration};

use rupi_core::{
  ReconciliationStatus, Tool, ToolError, ToolExecutionContext, ToolMetadata, ToolOutcome,
  ToolProgress, ToolRequest,
};
use serde_json::json;

use crate::{Deadline, Runtime, arg_str};

/// The non-idempotent append tool.
pub struct AppendTool {
  runtime: Runtime,
}

impl AppendTool {
  pub(crate) fn new(runtime: Runtime) -> Self {
    Self { runtime }
  }
}

impl Tool for AppendTool {
  fn metadata(&self) -> ToolMetadata {
    ToolMetadata::mutating(
      "append",
      "Append exact contents to a file. Completion is non-idempotent and cannot be guessed after interruption.",
      false,
    )
  }

  fn arguments_schema(&self) -> serde_json::Value {
    json!({
      "type": "object",
      "properties": {
        "path": { "type": "string", "description": "File to append, relative to the workspace." },
        "contents": { "type": "string", "description": "Exact bytes to append." }
      },
      "required": ["path", "contents"]
    })
  }

  fn preflight(&self, request: &ToolRequest) -> Result<(), ToolError> {
    let path = arg_str(request, "path")?;
    self
      .runtime
      .workspace
      .write_path(path)
      .map(|_| ())
      .map_err(|error| ToolError::new(error.to_string()))
  }

  fn execute(
    &self,
    request: &ToolRequest,
    progress: &mut dyn ToolProgress,
  ) -> Result<ToolOutcome, ToolError> {
    self.execute_inner(request, progress, &ToolExecutionContext::unbounded())
  }

  fn execute_with_context(
    &self,
    request: &ToolRequest,
    progress: &mut dyn ToolProgress,
    context: &ToolExecutionContext,
  ) -> Result<ToolOutcome, ToolError> {
    self.execute_inner(request, progress, context)
  }

  fn reconcile(&self, request: &ToolRequest) -> Result<ReconciliationStatus, ToolError> {
    let path = arg_str(request, "path")?;
    self
      .runtime
      .workspace
      .write_path(path)
      .map_err(|error| ToolError::new(error.to_string()))?;
    Ok(ReconciliationStatus::RequiresManualInspection {
      details: format!(
        "append call for '{}' is non-idempotent; inspect the file and do not infer commitment from a matching suffix",
        path
      ),
    })
  }
}

impl AppendTool {
  fn execute_inner(
    &self,
    request: &ToolRequest,
    _progress: &mut dyn ToolProgress,
    context: &ToolExecutionContext,
  ) -> Result<ToolOutcome, ToolError> {
    let path = arg_str(request, "path")?;
    let contents = arg_str(request, "contents")?;
    if context.is_cancelled_or_expired() {
      return Ok(ToolOutcome::failed(
        "append: cancelled before opening the file",
      ));
    }
    let resolved = self
      .runtime
      .workspace
      .write_path(path)
      .map_err(|error| ToolError::new(error.to_string()))?;
    if let Some(parent) = resolved.parent() {
      fs::create_dir_all(parent)
        .map_err(|error| ToolError::new(format!("append: cannot create parent: {error}")))?;
    }

    let deadline = Deadline::new(Duration::from_secs(30));
    let mut file =
      open_append(&resolved).map_err(|error| ToolError::after_start(format!("append: {error}")))?;
    file
      .write_all(contents.as_bytes())
      .map_err(|error| ToolError::after_start(format!("append: {error}")))?;
    if context.is_cancelled_or_expired() {
      return Ok(ToolOutcome::unknown(format!(
        "append to '{}' was interrupted; completion is unknown",
        resolved.display()
      )));
    }
    file
      .sync_all()
      .map_err(|error| ToolError::after_start(format!("append: {error}")))?;
    if context.is_cancelled_or_expired() {
      return Ok(ToolOutcome::unknown(format!(
        "append to '{}' completed at the filesystem boundary but the caller was interrupted; inspect before retrying",
        resolved.display()
      )));
    }

    Ok(ToolOutcome::succeeded(format!(
      "appended {} bytes to '{}' [in {} ms]",
      contents.len(),
      resolved.display(),
      deadline.elapsed_ms()
    )))
  }
}

fn open_append(path: &std::path::Path) -> Result<fs::File, std::io::Error> {
  let mut options = fs::OpenOptions::new();
  options.create(true).append(true).write(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_NOFOLLOW);
  }
  options.open(path)
}

#[cfg(test)]
mod tests {
  use rupi_core::{ToolCallId, ToolExecutionState};

  use super::*;
  use crate::{Workspace, testutil::Recorder};

  fn request(path: &str, contents: &str) -> ToolRequest {
    ToolRequest {
      call_id: ToolCallId::new(),
      name: "append".into(),
      arguments: json!({"path": path, "contents": contents}),
    }
  }

  #[test]
  fn appends_when_explicitly_requested() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("log.txt"), "first\n").unwrap();
    let tool = AppendTool::new(Runtime::new(Workspace::new(dir.path()).unwrap()));
    let mut recorder = Recorder::default();
    let outcome = tool
      .execute(&request("log.txt", "second\n"), &mut recorder)
      .unwrap();
    assert!(!outcome.is_error);
    assert_eq!(
      fs::read_to_string(dir.path().join("log.txt")).unwrap(),
      "first\nsecond\n"
    );
  }

  #[test]
  fn reconciliation_never_guesses_from_a_preexisting_suffix() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("log.txt"), "already DONE\n").unwrap();
    let tool = AppendTool::new(Runtime::new(Workspace::new(dir.path()).unwrap()));
    let status = tool.reconcile(&request("log.txt", "DONE\n")).unwrap();
    assert!(matches!(
      status,
      ReconciliationStatus::RequiresManualInspection { .. }
    ));
    assert_eq!(
      ToolExecutionState::Unknown.replay_decision(&tool.metadata()),
      rupi_core::ReplayDecision::ReconcileFirst
    );
  }

  #[cfg(unix)]
  #[test]
  fn outward_final_symlink_is_refused_before_append() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("important.txt");
    fs::write(&target, "keep").unwrap();
    std::os::unix::fs::symlink(&target, dir.path().join("link.txt")).unwrap();
    let tool = AppendTool::new(Runtime::new(Workspace::new(dir.path()).unwrap()));
    let mut recorder = Recorder::default();
    let error = tool
      .execute(&request("link.txt", "x"), &mut recorder)
      .unwrap_err();
    assert!(!error.started);
    assert!(error.message.contains("outside"), "{}", error.message);
    assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
  }

  #[cfg(unix)]
  #[test]
  fn outward_parent_symlink_is_refused_before_append() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
    let tool = AppendTool::new(Runtime::new(Workspace::new(dir.path()).unwrap()));
    let mut recorder = Recorder::default();
    let error = tool
      .execute(&request("link/secret.txt", "x"), &mut recorder)
      .unwrap_err();
    assert!(!error.started);
    assert!(error.message.contains("outside"), "{}", error.message);
    assert!(!outside.path().join("secret.txt").exists());
  }
}
