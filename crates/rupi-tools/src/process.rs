//! `process` — run a program directly, without shell parsing.
//!
//! This is the preferred boundary for invoking a known executable such as a
//! language interpreter or test runner. Arguments remain an argv list, so
//! spaces, quotes, and shell metacharacters are data rather than a second
//! command language. It shares `exec`'s timeout, cancellation, output, and
//! uncertain-completion lifecycle.

use std::{process::Command, time::Duration};

use rupi_core::{
  Tool, ToolError, ToolExecutionContext, ToolMetadata, ToolOutcome, ToolProgress, ToolRequest,
};
use serde_json::{Value, json};

use crate::{Deadline, Runtime, arg_str, exec::run_command};

/// Run one program with an explicit argument vector.
pub struct ProcessTool {
  runtime: Runtime,
}

impl ProcessTool {
  pub(crate) fn new(runtime: Runtime) -> Self {
    Self { runtime }
  }
}

impl Tool for ProcessTool {
  fn metadata(&self) -> ToolMetadata {
    ToolMetadata::mutating(
      "process",
      "Run a program directly with an argument list in the workspace; no shell quoting or expansion. Mutating and not idempotent.",
      false,
    )
  }

  fn arguments_schema(&self) -> Value {
    json!({
      "type": "object",
      "properties": {
        "program": { "type": "string", "description": "Executable to run directly; no shell is inserted." },
        "args": {
          "type": "array",
          "items": { "type": "string" },
          "description": "Arguments passed as argv values, without shell parsing."
        },
        "cwd": { "type": "string", "description": "Working directory, relative to the workspace." },
        "timeout_ms": { "type": "integer", "description": "Override the configured timeout." }
      },
      "required": ["program"]
    })
  }

  fn preflight(&self, request: &ToolRequest) -> Result<(), ToolError> {
    if let Some(cwd) = request.arguments.get("cwd").and_then(Value::as_str) {
      self
        .runtime
        .workspace
        .search_path(cwd)
        .map(|_| ())
        .map_err(|error| ToolError::new(error.to_string()))?;
    }
    Ok(())
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
}

impl ProcessTool {
  fn execute_inner(
    &self,
    request: &ToolRequest,
    progress: &mut dyn ToolProgress,
    context: &ToolExecutionContext,
  ) -> Result<ToolOutcome, ToolError> {
    let runtime = self.runtime.clone();
    let program = arg_str(request, "program")?;
    let args = arguments(request)?;
    let timeout_ms = crate::arg_u64(request, "timeout_ms").unwrap_or(runtime.shell_timeout_ms);
    let deadline = Deadline::new(Duration::from_millis(timeout_ms));
    let cwd = match request.arguments.get("cwd").and_then(Value::as_str) {
      Some(path) => runtime
        .workspace
        .search_path(path)
        .map_err(|error| ToolError::new(error.to_string()))?,
      None => runtime.workspace.root().to_path_buf(),
    };
    if !cwd.is_dir() {
      return Ok(ToolOutcome::failed(format!(
        "process: '{}' is not a directory",
        cwd.display()
      )));
    }

    let display = display_command(program, &args);
    let mut command = Command::new(program);
    command.args(&args);
    run_command(
      command, "process", &display, &cwd, &runtime, progress, &deadline, context,
    )
  }
}

fn arguments(request: &ToolRequest) -> Result<Vec<String>, ToolError> {
  let Some(values) = request.arguments.get("args") else {
    return Ok(Vec::new());
  };
  let Some(values) = values.as_array() else {
    return Err(ToolError::new(format!(
      "{}: argument 'args' must be an array of strings",
      request.name
    )));
  };
  values
    .iter()
    .enumerate()
    .map(|(index, value)| {
      value.as_str().map(str::to_owned).ok_or_else(|| {
        ToolError::new(format!(
          "{}: argument 'args[{index}]' must be a string",
          request.name
        ))
      })
    })
    .collect()
}

fn display_command(program: &str, args: &[String]) -> String {
  std::iter::once(program)
    .chain(args.iter().map(String::as_str))
    .collect::<Vec<_>>()
    .join(" ")
}

#[cfg(test)]
mod tests {
  use rupi_core::{Tool, ToolCallId, ToolExecutionState};

  use super::*;
  use crate::{Workspace, testutil::Recorder};

  fn request(arguments: Value) -> ToolRequest {
    ToolRequest {
      call_id: ToolCallId::new(),
      name: "process".into(),
      arguments,
    }
  }

  fn tool(dir: &tempfile::TempDir) -> ProcessTool {
    ProcessTool::new(Runtime::new(Workspace::new(dir.path()).unwrap()))
  }

  #[test]
  fn declares_a_direct_mutating_process_contract() {
    let dir = tempfile::tempdir().unwrap();
    let tool = tool(&dir);
    let metadata = tool.metadata();
    assert!(!metadata.read_only);
    assert!(!metadata.idempotent);
    assert!(metadata.description.contains("no shell"));
    assert_eq!(tool.arguments_schema()["required"], json!(["program"]));
  }

  #[test]
  fn runs_the_program_with_argv_values() {
    let dir = tempfile::tempdir().unwrap();
    let mut recorder = Recorder::default();
    let program = std::env::current_exe().unwrap();
    let outcome = tool(&dir)
      .execute(
        &request(json!({
          "program": program,
          "args": ["--list"]
        })),
        &mut recorder,
      )
      .unwrap();
    assert_eq!(
      outcome.state,
      ToolExecutionState::Succeeded,
      "{}",
      outcome.text
    );
    assert_eq!(outcome.status, Some(0));
    assert!(outcome.text.contains("runs_the_program_with_argv_values"));
  }

  #[test]
  fn rejects_a_non_string_argv_value_before_spawning() {
    let dir = tempfile::tempdir().unwrap();
    let mut recorder = Recorder::default();
    let error = tool(&dir)
      .execute(
        &request(json!({"program": "definitely-not-used", "args": [1]})),
        &mut recorder,
      )
      .unwrap_err();
    assert!(!error.started);
    assert!(error.message.contains("args[0]"), "{}", error.message);
  }
}
