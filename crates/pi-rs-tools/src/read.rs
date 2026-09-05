//! `read` — inspect a file.
//!
//! Read is the tool most likely to be pointed at a large file, so it is
//! line-oriented with an explicit window, and it protects the context in three
//! ways: a byte cap per line, a cap on the number of lines returned, and
//! reduction of the assembled result. It also refuses binary content with an
//! explanation instead of shipping control characters to the model.

use std::{
  fs::File,
  io::{BufRead, BufReader},
};

use pi_rs_core::{Tool, ToolError, ToolMetadata, ToolOutcome, ToolProgress, ToolRequest};
use serde_json::json;

use crate::{Deadline, Runtime, arg_str};

/// Default number of lines returned by one call.
const DEFAULT_LIMIT: u64 = 2_000;
/// A hard ceiling so an explicit request cannot ask for an unbounded window.
const MAX_LIMIT: u64 = 20_000;
/// Read is a local scan; it should finish far inside a turn.
const READ_BUDGET: std::time::Duration = std::time::Duration::from_secs(60);

/// The `read` tool.
pub struct ReadTool {
  runtime: Runtime,
}

impl ReadTool {
  pub fn new(runtime: Runtime) -> Self {
    Self { runtime }
  }
}

impl Tool for ReadTool {
  fn metadata(&self) -> ToolMetadata {
    ToolMetadata::read_only(
      "read",
      "Read a text file with line numbers. Use offset and limit for large files.",
    )
  }

  fn arguments_schema(&self) -> serde_json::Value {
    json!({
      "type": "object",
      "properties": {
        "path": { "type": "string", "description": "File to read, relative to the workspace." },
        "offset": { "type": "integer", "description": "1-based first line. Defaults to 1." },
        "limit": { "type": "integer", "description": "Max lines. Defaults to 2000." }
      },
      "required": ["path"]
    })
  }

  fn execute(
    &self,
    request: &ToolRequest,
    progress: &mut dyn ToolProgress,
  ) -> Result<ToolOutcome, ToolError> {
    let runtime = self.runtime.clone();
    let path = arg_str(request, "path")?;
    let resolved = runtime
      .workspace
      .read_path(path)
      .map_err(|error| ToolError::new(error.to_string()))?;
    let offset: usize = request
      .arguments
      .get("offset")
      .and_then(|v| v.as_u64())
      .unwrap_or(1)
      .max(1)
      .try_into()
      .unwrap_or(1);
    let limit = request
      .arguments
      .get("limit")
      .and_then(|v| v.as_u64())
      .unwrap_or(DEFAULT_LIMIT)
      .clamp(1, MAX_LIMIT);

    let file = File::open(&resolved).map_err(|error| {
      ToolError::new(format!(
        "read: cannot open '{}': {error}",
        resolved.display()
      ))
    })?;
    let mut reader = BufReader::new(file);
    let deadline = Deadline::new(READ_BUDGET);

    let mut out = String::new();
    let mut line_number = 0usize;
    let mut emitted: usize = 0;
    let mut truncated_by_limit = false;
    let mut stopped_early = false;
    let mut binary_at: Option<usize> = None;
    let mut line = String::new();

    loop {
      line.clear();
      let read = reader
        .read_line(&mut line)
        .map_err(|error| ToolError::new(format!("read: io error: {error}")))?;
      if read == 0 {
        break;
      }
      line_number += 1;
      if line.contains('\0') {
        binary_at = Some(line_number);
        break;
      }
      if line_number < offset {
        continue;
      }
      if emitted >= limit as usize {
        truncated_by_limit = true;
        break;
      }
      let mut display = line.trim_end_matches(['\n', '\r']);
      let capped = display.len() > runtime.max_line_bytes;
      if capped {
        let mut cut = runtime.max_line_bytes;
        while cut > 0 && !display.is_char_boundary(cut) {
          cut -= 1;
        }
        display = &display[..cut];
      }
      out.push_str(&format!(
        "{line_number:>5}: {display}{}\n",
        if capped { "…[line truncated]" } else { "" }
      ));
      emitted += 1;
      if deadline.expired() {
        stopped_early = true;
        break;
      }
    }

    if let Some(line_number) = binary_at {
      return Ok(ToolOutcome::failed(format!(
        "'{}' looks binary: a NUL byte appeared at line {line_number}. \
         Read a text file, or use a tool that handles binaries.",
        resolved.display()
      )));
    }

    let mut notes = Vec::new();
    if truncated_by_limit {
      notes.push(format!(
        "stopped at {limit} lines; re-read with offset {}",
        offset + limit as usize
      ));
    }
    if stopped_early {
      notes.push(format!(
        "stopped after {}s at line {line_number}; re-read with offset {}",
        READ_BUDGET.as_secs(),
        line_number + 1
      ));
    }
    if line_number == 0 {
      notes.push("file is empty".to_string());
    }
    if !notes.is_empty() {
      out.push_str(&format!("\n[{}]\n", notes.join("; ")));
    }

    crate::BoundedProgress::new(progress, 64 * 1024, deadline).send(&out);
    Ok(runtime.finish(out))
  }
}

#[cfg(test)]
mod tests {
  use pi_rs_core::ToolCallId;

  use super::*;
  use crate::testutil::{Recorder, runtime};

  fn request(path: &str, extra: serde_json::Value) -> ToolRequest {
    let mut arguments = serde_json::Map::new();
    arguments.insert("path".into(), json!(path));
    if let Some(map) = extra.as_object() {
      for (k, v) in map {
        arguments.insert(k.clone(), v.clone());
      }
    }
    ToolRequest {
      call_id: ToolCallId::new(),
      name: "read".into(),
      arguments: serde_json::Value::Object(arguments),
    }
  }

  fn read(dir: &tempfile::TempDir, path: &str, extra: serde_json::Value) -> ToolOutcome {
    let tool = ReadTool::new(runtime(dir));
    let mut recorder = Recorder::default();
    tool.execute(&request(path, extra), &mut recorder).unwrap()
  }

  #[test]
  fn returns_numbered_lines() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
    let outcome = read(&dir, "a.txt", json!({}));
    assert!(outcome.text.contains("1: one"), "{}", outcome.text);
    assert!(outcome.text.contains("3: three"), "{}", outcome.text);
    assert!(!outcome.is_error);
  }

  #[test]
  fn honours_offset_and_limit() {
    let dir = tempfile::tempdir().unwrap();
    let body: String = (1..=500).map(|i| format!("line {i}\n")).collect();
    std::fs::write(dir.path().join("big.txt"), &body).unwrap();
    let outcome = read(&dir, "big.txt", json!({"offset": 100, "limit": 3}));
    assert!(outcome.text.contains("100: line 100"), "{}", outcome.text);
    assert!(outcome.text.contains("102: line 102"), "{}", outcome.text);
    assert!(!outcome.text.contains("103: line 103"), "{}", outcome.text);
    assert!(
      outcome.text.contains("re-read with offset 103"),
      "tells how to continue"
    );
  }

  #[test]
  fn a_path_that_cannot_be_read_is_a_failure_not_an_empty_success() {
    let dir = tempfile::tempdir().unwrap();
    let tool = ReadTool::new(runtime(&dir));
    let mut recorder = Recorder::default();
    let error = match tool.execute(&request("missing.txt", json!({})), &mut recorder) {
      Err(error) => error,
      Ok(_) => panic!("open failure"),
    };
    assert!(!error.started);
    assert!(error.message.contains("cannot open"), "{}", error.message);
  }

  #[test]
  fn refuses_binary_content_with_an_explanation() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("blob.bin"), "head\u{0}tail").unwrap();
    let outcome = read(&dir, "blob.bin", json!({}));
    assert!(outcome.is_error);
    assert!(outcome.text.contains("looks binary"), "{}", outcome.text);
    assert!(
      !outcome.text.contains('\u{0}'),
      "the NUL must not reach the model"
    );
  }

  #[test]
  fn long_lines_are_capped_per_line() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("wide.txt"), "y".repeat(50_000)).unwrap();
    let outcome = read(&dir, "wide.txt", json!({}));
    assert!(
      outcome.text.contains("[line truncated]"),
      "{}",
      &outcome.text[..80]
    );
    assert!(outcome.text.len() < 6_000, "{}", outcome.text.len());
  }

  #[test]
  fn a_large_file_is_reduced_not_dumped() {
    let dir = tempfile::tempdir().unwrap();
    let body: String = (1..=20_000)
      .map(|i| format!("line {i}: payload\n"))
      .collect();
    std::fs::write(dir.path().join("huge.txt"), &body).unwrap();
    let outcome = read(&dir, "huge.txt", json!({}));
    assert!(outcome.reduced, "reduced flag must be set");
    // The visible form must fit the budget *including* the elision notice.
    assert!(outcome.text.len() < 9 * 1024, "{}", outcome.text.len());
    assert!(outcome.text.contains("bytes elided"));
  }

  #[test]
  fn reduction_keeps_both_ends_of_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let body: String = (1..=20_000)
      .map(|i| format!("line {i}: payload\n"))
      .collect();
    std::fs::write(dir.path().join("h2.txt"), &body).unwrap();
    let outcome = read(&dir, "h2.txt", json!({}));
    // The point of reduction is context protection, so the visible form must be
    // strictly smaller than the source it replaced.
    assert!(outcome.text.len() < body.len());
    assert!(outcome.text.contains("line 1: payload"), "head retained");
    assert!(
      outcome.text.contains("line 2000: payload"),
      "window tail retained"
    );
    assert!(
      outcome.text.contains("stopped at 2000 lines"),
      "tells where the window ended: {}",
      &outcome.text[..200]
    );
    assert!(outcome.text.contains("full output is archived"));
  }

  #[test]
  fn the_result_is_streamed_as_well_as_returned() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("s.txt"), "hello\n").unwrap();
    let tool = ReadTool::new(runtime(&dir));
    let mut recorder = Recorder::default();
    let outcome = tool
      .execute(&request("s.txt", json!({})), &mut recorder)
      .unwrap();
    assert_eq!(recorder.text(), outcome.text);
  }

  #[test]
  fn empty_file_says_so() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("e.txt"), "").unwrap();
    let outcome = read(&dir, "e.txt", json!({}));
    assert!(outcome.text.contains("empty"), "{}", outcome.text);
  }

  #[test]
  fn read_only_metadata_allows_replay() {
    use pi_rs_core::{ReplayDecision, ToolExecutionState};
    let tool = ReadTool::new(Runtime::new(crate::Workspace::new("/tmp").unwrap()));
    let meta = tool.metadata();
    assert!(meta.read_only);
    assert_eq!(
      ToolExecutionState::Started.replay_decision(&meta),
      ReplayDecision::Replay
    );
  }
}
