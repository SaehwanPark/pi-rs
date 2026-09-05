//! `write` — create or replace a file.
//!
//! Two deliberate choices make this tool safe enough to expose to a model:
//!
//! - **Atomic replace.** Content is written to a sibling temporary file and then
//!   renamed over the target. A crash mid-write leaves the previous file intact
//!   rather than a half-written one, which matters because the next thing that
//!   reads the path may be the model itself.
//! - **Refusal, not truncation.** A path outside the workspace is refused with
//!   the resolved path in the message, so the trace states exactly which boundary
//!   was hit.

use std::{
  fs::{self, File},
  io::Write,
};

use pi_rs_core::{Tool, ToolError, ToolMetadata, ToolOutcome, ToolProgress, ToolRequest};
use serde_json::json;

use crate::{Deadline, Runtime, arg_str};

/// The `write` tool.
pub struct WriteTool {
  runtime: Runtime,
}

impl WriteTool {
  pub(crate) fn new(runtime: Runtime) -> Self {
    Self { runtime }
  }
}

impl Tool for WriteTool {
  fn metadata(&self) -> ToolMetadata {
    // Idempotent: writing the same bytes twice converges to the same state.
    ToolMetadata::mutating(
      "write",
      "Create or replace a file with exact contents.",
      true,
    )
  }

  fn arguments_schema(&self) -> serde_json::Value {
    json!({
      "type": "object",
      "properties": {
        "path": { "type": "string", "description": "File to write, relative to the workspace." },
        "contents": { "type": "string", "description": "Exact file contents." },
        "append": { "type": "boolean", "description": "Append instead of replacing. Defaults to false." }
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
    _progress: &mut dyn ToolProgress,
  ) -> Result<ToolOutcome, ToolError> {
    let runtime = self.runtime.clone();
    let path = arg_str(request, "path")?;
    let contents = arg_str(request, "contents")?;
    let append = crate::arg_bool(request, "append", false);
    let deadline = Deadline::new(crate::WRITE_BUDGET);

    let resolved = runtime
      .workspace
      .write_path(path)
      .map_err(|error| ToolError::new(error.to_string()))?;
    if let Some(parent) = resolved.parent() {
      fs::create_dir_all(parent)
        .map_err(|error| ToolError::new(format!("write: cannot create parent: {error}")))?;
    }

    let existing = fs::metadata(&resolved).map(|m| m.len()).ok();
    let bytes = contents.as_bytes();

    if append {
      let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&resolved)
        .map_err(after_start)?;
      file
        .write_all(bytes)
        .map_err(after_start)
        .and_then(|_| file.sync_all().map_err(after_start))?;
    } else {
      // Sibling temp file, then rename: the target is either the old file or the
      // new one, never a mixture.
      let temp = temp_path_for(&resolved);
      {
        let mut file = File::create(&temp).map_err(after_start)?;
        file
          .write_all(bytes)
          .map_err(|error| {
            let _ = fs::remove_file(&temp);
            after_start(error)
          })
          .and_then(|_| file.sync_all().map_err(after_start))?;
      }
      fs::rename(&temp, &resolved).map_err(|error| {
        let _ = fs::remove_file(&temp);
        after_start(error)
      })?;
    }

    let verb = if append { "appended to" } else { "wrote" };
    let mut outcome = ToolOutcome::succeeded(format!(
      "{verb} {} bytes to '{}'{}",
      bytes.len(),
      resolved.display(),
      existing
        .map(|len| format!(" (replaced a {} byte file)", len))
        .unwrap_or_else(|| " (created)".to_string())
    ));
    outcome
      .text
      .push_str(&format!(" [in {} ms]", deadline.elapsed_ms()));
    Ok(outcome)
  }
}

/// A temporary path next to the target, so the rename stays on one filesystem.
pub(crate) fn temp_path_for(target: &std::path::Path) -> std::path::PathBuf {
  let name = target
    .file_name()
    .map(|n| n.to_string_lossy().to_string())
    .unwrap_or_else(|| "file".to_string());
  let parent = target.parent().unwrap_or_else(|| std::path::Path::new("."));
  parent.join(format!(
    ".{name}.{}.{}.tmp",
    std::process::id(),
    std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .map(|d| d.subsec_nanos())
      .unwrap_or(0)
  ))
}

fn after_start(error: std::io::Error) -> ToolError {
  // After the first byte, a failure does not prove the file is unchanged.
  ToolError::after_start(format!("write: {error}"))
}

#[cfg(test)]
mod tests {
  use pi_rs_core::{ToolCallId, ToolExecutionState};

  use super::*;
  use crate::testutil::{Recorder, runtime};

  fn request(path: &str, contents: &str, append: bool) -> ToolRequest {
    let mut arguments = serde_json::Map::new();
    arguments.insert("path".into(), json!(path));
    arguments.insert("contents".into(), json!(contents));
    if append {
      arguments.insert("append".into(), json!(true));
    }
    ToolRequest {
      call_id: ToolCallId::new(),
      name: "write".into(),
      arguments: serde_json::Value::Object(arguments),
    }
  }

  fn write(dir: &tempfile::TempDir, path: &str, contents: &str) -> ToolOutcome {
    let tool = WriteTool::new(runtime(dir));
    let mut recorder = Recorder::default();
    tool
      .execute(&request(path, contents, false), &mut recorder)
      .unwrap()
  }

  #[test]
  fn creates_files_with_exact_contents() {
    let dir = tempfile::tempdir().unwrap();
    let outcome = write(&dir, "src/new.rs", "fn main() {}\n");
    assert!(!outcome.is_error, "{}", outcome.text);
    assert_eq!(
      fs::read_to_string(dir.path().join("src/new.rs")).unwrap(),
      "fn main() {}\n"
    );
    assert!(outcome.text.contains("created"));
  }

  #[test]
  fn replaces_and_reports_the_previous_size() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "old content is longe").unwrap();
    let outcome = write(&dir, "a.txt", "new");
    assert!(
      outcome.text.contains("replaced a 20 byte file"),
      "{}",
      outcome.text
    );
    assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "new");
  }

  #[test]
  fn appends_when_asked() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("log.txt"), "first\n").unwrap();
    let tool = WriteTool::new(runtime(&dir));
    let mut recorder = Recorder::default();
    let outcome = tool
      .execute(&request("log.txt", "second\n", true), &mut recorder)
      .unwrap();
    assert!(!outcome.is_error);
    assert_eq!(
      fs::read_to_string(dir.path().join("log.txt")).unwrap(),
      "first\nsecond\n"
    );
  }

  #[test]
  fn refuses_paths_outside_the_workspace_before_touching_anything() {
    let dir = tempfile::tempdir().unwrap();
    let tool = WriteTool::new(runtime(&dir));
    let mut recorder = Recorder::default();
    let error = tool
      .execute(
        &request("/tmp/definitely-not-mine-pi-rs", "x", false),
        &mut recorder,
      )
      .expect_err("boundary refusal");
    assert!(
      !error.started,
      "a boundary refusal happens before the first byte"
    );
    assert!(error.message.contains("outside"), "{}", error.message);
    assert!(!std::path::Path::new("/tmp/definitely-not-mine-pi-rs").exists());
  }

  #[test]
  fn leaves_no_temporary_files_behind() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir, "a.txt", "hello");
    let leftovers: Vec<_> = fs::read_dir(dir.path())
      .unwrap()
      .filter_map(|e| e.ok())
      .map(|e| e.file_name().to_string_lossy().to_string())
      .filter(|n| n.ends_with(".tmp"))
      .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
  }

  #[test]
  fn is_declared_mutating_and_idempotent() {
    let tool = WriteTool::new(Runtime::new(
      crate::Workspace::new(std::env::temp_dir()).unwrap(),
    ));
    let meta = tool.metadata();
    assert!(!meta.read_only);
    assert!(
      meta.idempotent,
      "same bytes twice converge to the same state"
    );
  }

  #[test]
  fn a_write_into_a_non_directory_parent_fails_before_starting() {
    // The parent creation step is the first effect; if it fails, the target file
    // is provably untouched, so this is a clean failure and not `Unknown`.
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("blocker"), "x").unwrap();
    let tool = WriteTool::new(runtime(&dir));
    let mut recorder = Recorder::default();
    let error = tool
      .execute(&request("blocker/nested.txt", "hi", false), &mut recorder)
      .expect_err("parent creation failure");
    assert!(!error.started);
    assert_eq!(
      ToolExecutionState::Failed,
      error.implied_state(&tool.metadata()),
      "nothing was written, so the effect is not uncertain"
    );
  }

  #[test]
  fn missing_argument_is_refused_before_execution() {
    let dir = tempfile::tempdir().unwrap();
    let tool = WriteTool::new(runtime(&dir));
    let mut recorder = Recorder::default();
    let error = tool
      .execute(
        &ToolRequest {
          call_id: ToolCallId::new(),
          name: "write".into(),
          arguments: json!({"path": "x"}),
        },
        &mut recorder,
      )
      .expect_err("argument validation failure");
    assert!(error.message.contains("contents"), "{}", error.message);
  }
}
