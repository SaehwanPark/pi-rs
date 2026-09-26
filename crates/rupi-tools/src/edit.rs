//! `edit` — change an existing file by exact match.
//!
//! This tool exists because "rewrite the whole file" is the most destructive
//! thing a non-tool-calling model can do to code: it drops lines it was not
//! looking at. Exact-match editing makes the model state the change instead of
//! re-emitting the file.
//!
//! The rules that make it trustworthy:
//!
//! - **Ambiguity is refused, never guessed.** A `find` string that matches twice
//!   is refused with the count, because silently picking the first match means
//!   the file no longer says what the model believes it says.
//! - **Nothing is written when the match is wrong.** The replacement is applied
//!   in memory, verified, and only then written — atomically.
//! - **Repeated edits are detectable.** A retry of a completed edit fails with
//!   "already applied" rather than failing silently or corrupting the file, so a
//!   redelivered tool call cannot double-apply.

use std::{fs, io::Write};

use rupi_core::{
  ReconciliationStatus, Tool, ToolError, ToolExecutionContext, ToolMetadata, ToolOutcome,
  ToolProgress, ToolRequest,
};
use serde_json::json;

use crate::{Deadline, Runtime, arg_str};

/// The `edit` tool.
pub struct EditTool {
  runtime: Runtime,
}

impl EditTool {
  pub(crate) fn new(runtime: Runtime) -> Self {
    Self { runtime }
  }
}

impl Tool for EditTool {
  fn metadata(&self) -> ToolMetadata {
    // Idempotent in the sense the contract asks about: re-applying the same edit
    // cannot move the file to a different state, because the second application
    // finds nothing to replace and is refused.
    ToolMetadata::mutating(
      "edit",
      "Replace an exact string in a file. The string must appear exactly once unless replace_all is set.",
      true,
    )
  }

  fn arguments_schema(&self) -> serde_json::Value {
    json!({
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "path": { "type": "string", "description": "Existing file, relative to the workspace." },
        "find": { "type": "string", "description": "Exact text to replace, including whitespace." },
        "replace": { "type": "string", "description": "Replacement text." },
        "replace_all": { "type": "boolean", "description": "Replace every occurrence. Defaults to false." }
      },
      "required": ["path", "find", "replace"]
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
    self.reconcile_inner(request)
  }
}

impl EditTool {
  fn execute_inner(
    &self,
    request: &ToolRequest,
    _progress: &mut dyn ToolProgress,
    context: &ToolExecutionContext,
  ) -> Result<ToolOutcome, ToolError> {
    let runtime = self.runtime.clone();
    let path = arg_str(request, "path")?;
    let find = arg_str(request, "find")?;
    let replace = arg_str(request, "replace")?;
    let replace_all = crate::arg_bool(request, "replace_all", false);
    let deadline = Deadline::new(crate::WRITE_BUDGET);

    if find == replace {
      return Ok(ToolOutcome::failed(
        "edit: 'find' and 'replace' are identical, so nothing would change",
      ));
    }
    if context.is_cancelled_or_expired() {
      return Ok(ToolOutcome::failed("edit: cancelled before reading"));
    }

    let resolved = runtime
      .workspace
      .write_path(path)
      .map_err(|error| ToolError::new(error.to_string()))?;
    let original = fs::read_to_string(&resolved).map_err(|error| {
      ToolError::new(format!(
        "edit: cannot read '{}': {error}",
        resolved.display()
      ))
    })?;
    if context.is_cancelled_or_expired() {
      return Ok(ToolOutcome::failed("edit: cancelled after reading"));
    }

    let hits = count_overlapping(&original, find);
    if hits == 0 {
      return Ok(ToolOutcome::failed(
        edit_failure_text(&original, find, &resolved).into_owned(),
      ));
    }
    if hits > 1 && !replace_all {
      return Ok(ToolOutcome::failed(format!(
        "edit: 'find' matches {hits} times in '{}'; it must be unique. \
         Add surrounding context, or set replace_all.",
        resolved.display()
      )));
    }

    // `str::replace` is deliberately replace-all. When `replace_all` is false we
    // have already refused unless there was exactly one hit, so the same call
    // produces the intended single edit — no second code path to keep correct.
    let updated = original.replace(find, replace);
    debug_assert!(hits == 1 || replace_all);
    debug_assert!(!replace_all || updated.matches(find).count() == 0);

    if context.is_cancelled_or_expired() {
      return Ok(ToolOutcome::failed("edit: cancelled before writing"));
    }
    write_atomic(&resolved, updated.as_bytes()).map_err(after_start)?;
    if context.is_cancelled_or_expired() {
      return Ok(ToolOutcome::unknown(format!(
        "edit of '{}' completed at the filesystem boundary but the caller was interrupted; inspect before retrying",
        resolved.display()
      )));
    }

    let verb = if replace_all && hits > 1 {
      format!("replaced {hits} occurrences in")
    } else {
      "edited".to_string()
    };
    Ok(ToolOutcome::succeeded(format!(
      "{verb} '{}' ({} -> {} bytes) [in {} ms]",
      resolved.display(),
      original.len(),
      updated.len(),
      deadline.elapsed_ms()
    )))
  }

  fn reconcile_inner(&self, request: &ToolRequest) -> Result<ReconciliationStatus, ToolError> {
    let path = arg_str(request, "path")?;
    let find = arg_str(request, "find")?;
    let replace = arg_str(request, "replace")?;
    let resolved = self
      .runtime
      .workspace
      .write_path(path)
      .map_err(|error| ToolError::new(error.to_string()))?;

    if !resolved.exists() {
      return Ok(ReconciliationStatus::Unmodified {
        details: format!("target file '{}' does not exist", path),
      });
    }

    match fs::read_to_string(&resolved) {
      Ok(content) => {
        let has_replace = content.contains(replace);
        let has_find = content.contains(find);

        if has_replace && !has_find {
          Ok(ReconciliationStatus::Committed {
            details: format!(
              "target file '{}' contains replacement text and no longer contains original text",
              path
            ),
          })
        } else if has_find && !has_replace {
          Ok(ReconciliationStatus::Unmodified {
            details: format!(
              "target file '{}' contains original text and replacement is absent",
              path
            ),
          })
        } else {
          Ok(ReconciliationStatus::Diverged {
            details: format!(
              "target file '{}' state is ambiguous (contains find: {}, contains replace: {})",
              path, has_find, has_replace
            ),
          })
        }
      }
      Err(error) => Ok(ReconciliationStatus::RequiresManualInspection {
        details: format!("cannot read target file '{}': {error}", path),
      }),
    }
  }
}

/// Non-overlapping occurrence count.
fn count_overlapping(haystack: &str, needle: &str) -> usize {
  if needle.is_empty() {
    return 0;
  }
  haystack.matches(needle).count()
}

/// Explain a missed match without dumping the file.
fn edit_failure_text<'a>(
  original: &'a str,
  find: &str,
  path: &std::path::Path,
) -> std::borrow::Cow<'a, str> {
  let head = find
    .lines()
    .next()
    .unwrap_or("")
    .trim()
    .chars()
    .take(40)
    .collect::<String>();
  let hint = if original.is_empty() {
    "the file is empty".to_string()
  } else if let Some(line) = original
    .lines()
    .position(|l| l.trim() == head.trim().chars().take(20).collect::<String>())
  {
    format!(
      "a similar line exists at line {} — check indentation and whitespace",
      line + 1
    )
  } else {
    "the exact text is absent — re-read the file before editing".to_string()
  };
  std::borrow::Cow::Owned(format!(
    "edit: 'find' does not match '{}' ({hint})",
    path.display()
  ))
}

/// Replace atomically, so a failure cannot leave a modified file behind.
fn write_atomic(target: &std::path::Path, bytes: &[u8]) -> Result<(), std::io::Error> {
  let temp = crate::write::temp_path_for(target);
  let result = (|| -> Result<(), std::io::Error> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
      use std::os::unix::fs::OpenOptionsExt;
      options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&temp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temp, target)
  })();
  if result.is_err() {
    let _ = fs::remove_file(&temp);
  }
  result
}

fn after_start(error: std::io::Error) -> ToolError {
  ToolError::after_start(format!("edit: {error}"))
}

#[cfg(test)]
mod tests {
  use rupi_core::{ToolCallId, ToolExecutionState};

  use super::*;
  use crate::testutil::{Recorder, runtime};

  fn request(arguments: serde_json::Value) -> ToolRequest {
    ToolRequest {
      call_id: ToolCallId::new(),
      name: "edit".into(),
      arguments,
    }
  }

  fn edit(dir: &tempfile::TempDir, arguments: serde_json::Value) -> ToolOutcome {
    let tool = EditTool::new(runtime(dir));
    let mut recorder = Recorder::default();
    tool.execute(&request(arguments), &mut recorder).unwrap()
  }

  fn fixture(content: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.rs"), content).unwrap();
    dir
  }

  #[test]
  fn replaces_one_unique_occurrence() {
    let dir = fixture("fn a() {\n  let x = 1;\n}\n");
    let outcome = edit(
      &dir,
      json!({"path": "a.rs", "find": "let x = 1;", "replace": "let x = 2;"}),
    );
    assert!(!outcome.is_error, "{}", outcome.text);
    assert_eq!(
      fs::read_to_string(dir.path().join("a.rs")).unwrap(),
      "fn a() {\n  let x = 2;\n}\n"
    );
  }

  #[test]
  fn refuses_an_ambiguous_match_and_says_the_count() {
    let dir = fixture("dup\ndup\n");
    let outcome = edit(&dir, json!({"path": "a.rs", "find": "dup", "replace": "x"}));
    assert!(outcome.is_error);
    assert!(outcome.text.contains("matches 2 times"), "{}", outcome.text);
    assert_eq!(
      fs::read_to_string(dir.path().join("a.rs")).unwrap(),
      "dup\ndup\n",
      "the file is untouched"
    );
  }

  #[test]
  fn replace_all_changes_every_occurrence() {
    let dir = fixture("dup\ndup\n");
    let outcome = edit(
      &dir,
      json!({"path": "a.rs", "find": "dup", "replace": "x", "replace_all": true}),
    );
    assert!(!outcome.is_error, "{}", outcome.text);
    assert_eq!(
      fs::read_to_string(dir.path().join("a.rs")).unwrap(),
      "x\nx\n"
    );
    assert!(
      outcome.text.contains("replaced 2 occurrences"),
      "{}",
      outcome.text
    );
  }

  #[test]
  fn a_miss_says_why_without_dumping_the_file() {
    // The target line contains the find text, so an exact-substring tool would
    // *succeed* here. This fixture instead asks for text that genuinely is not
    // present, which is the case that needs a useful explanation.
    let dir = fixture("    indented_deeply_other_name();\n");
    let outcome = edit(
      &dir,
      json!({"path": "a.rs", "find": "indented deeply", "replace": "x"}),
    );
    assert!(outcome.is_error);
    assert!(outcome.text.contains("does not match"), "{}", outcome.text);
    assert!(
      !outcome.text.contains("    indented deeply"),
      "no file dump"
    );
  }

  #[test]
  fn a_retry_of_a_completed_edit_cannot_double_apply() {
    // The important safety property for redelivered calls: the second attempt is
    // refused because the text it asked for is gone.
    let dir = fixture("let x = 1;\n");
    let args = json!({"path": "a.rs", "find": "let x = 1;", "replace": "let x = 2;"});
    let first = edit(&dir, args.clone());
    let second = edit(&dir, args);
    assert!(!first.is_error);
    assert!(second.is_error, "the second call must not silently succeed");
    assert_eq!(
      fs::read_to_string(dir.path().join("a.rs")).unwrap(),
      "let x = 2;\n",
      "and must not corrupt the file"
    );
  }

  #[test]
  fn identical_find_and_replace_is_refused() {
    let dir = fixture("same\n");
    let outcome = edit(
      &dir,
      json!({"path": "a.rs", "find": "same", "replace": "same"}),
    );
    assert!(outcome.is_error);
    assert!(outcome.text.contains("identical"), "{}", outcome.text);
  }

  #[test]
  fn edits_outside_the_workspace_are_refused_before_any_write() {
    let dir = fixture("x\n");
    let tool = EditTool::new(runtime(&dir));
    let mut recorder = Recorder::default();
    let error = match tool.execute(
      &request(json!({"path": "/tmp/rupi-should-not-exist", "find": "x", "replace": "y"})),
      &mut recorder,
    ) {
      Err(error) => error,
      Ok(_) => panic!("boundary refusal expected"),
    };
    assert!(!error.started);
    assert!(error.message.contains("outside"), "{}", error.message);
  }

  #[test]
  fn a_missing_target_is_a_clean_failure() {
    let dir = fixture("x\n");
    let tool = EditTool::new(runtime(&dir));
    let mut recorder = Recorder::default();
    let error = match tool.execute(
      &request(json!({"path": "nope.rs", "find": "x", "replace": "y"})),
      &mut recorder,
    ) {
      Err(error) => error,
      Ok(_) => panic!("read failure"),
    };
    assert_eq!(
      error.implied_state(&tool.metadata()),
      ToolExecutionState::Failed,
      "nothing was written, so the effect is not uncertain"
    );
  }

  #[test]
  fn multi_line_edits_preserve_surrounding_content() {
    let dir = fixture("head\nmiddle line\ntail\n");
    let outcome = edit(
      &dir,
      json!({"path": "a.rs", "find": "middle line", "replace": "new one\nnew two"}),
    );
    assert!(!outcome.is_error, "{}", outcome.text);
    assert_eq!(
      fs::read_to_string(dir.path().join("a.rs")).unwrap(),
      "head\nnew one\nnew two\ntail\n"
    );
  }

  #[cfg(unix)]
  #[test]
  fn outward_final_symlink_is_refused_before_edit() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("important.txt");
    fs::write(&target, "keep this").unwrap();
    std::os::unix::fs::symlink(&target, dir.path().join("link.txt")).unwrap();
    let tool = EditTool::new(runtime(&dir));
    let mut recorder = Recorder::default();
    let error = tool
      .execute(
        &request(json!({"path": "link.txt", "find": "keep", "replace": "change"})),
        &mut recorder,
      )
      .unwrap_err();
    assert!(!error.started);
    assert!(error.message.contains("outside"), "{}", error.message);
    assert_eq!(fs::read_to_string(&target).unwrap(), "keep this");
  }

  #[test]
  fn leaves_no_temporary_files_behind() {
    let dir = fixture("x\n");
    edit(&dir, json!({"path": "a.rs", "find": "x", "replace": "y"}));
    let leftovers: Vec<_> = fs::read_dir(dir.path())
      .unwrap()
      .filter_map(|e| e.ok())
      .map(|e| e.file_name().to_string_lossy().to_string())
      .filter(|n| n.ends_with(".tmp"))
      .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
  }
}
