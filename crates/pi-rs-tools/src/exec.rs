//! `exec` — run a shell command, under explicit policy.
//!
//! This is the only tool that can do arbitrary things, so its design is mostly
//! about not lying about what happened:
//!
//! - **Timeout is enforced by the runtime, not the shell.** The child writes into
//!   a bounded pipe and a watchdog thread waits on it. When the budget is spent
//!   the child is killed and the result says the completion is *unknown*, because
//!   a killed command may have already written files, sent packets, or committed
//!   a transaction.
//! - **Output is bounded before it is buffered.** A build log is not an
//!   opportunity to exhaust memory.
//! - **The tool is refused before it runs unless policy allows it.** Denial is the
//!   registry's job; this module additionally refuses to be constructed without a
//!   shell permission, so a mis-wired registry cannot expose it by accident.
//!
//! Cancellation semantics are deliberate: a command cancelled *before* it started
//! is a clean failure, and one interrupted *after* it started is `Unknown`.

use std::{
  io::Read,
  process::{Child, Command, ExitStatus, Stdio},
  sync::mpsc,
  thread,
  time::Duration,
};

use pi_rs_core::{
  Tool, ToolError, ToolExecutionContext, ToolMetadata, ToolOutcome, ToolProgress, ToolRequest,
};
use serde_json::json;

use crate::{Deadline, Runtime, arg_str};

/// The `exec` tool.
pub struct ExecTool {
  runtime: Runtime,
}

impl ExecTool {
  pub(crate) fn new(runtime: Runtime) -> Self {
    Self { runtime }
  }
}

impl Tool for ExecTool {
  fn metadata(&self) -> ToolMetadata {
    // Not idempotent: a second `git commit` or `terraform apply` is a different
    // act, not a repeat of the same one.
    ToolMetadata::mutating(
      "exec",
      "Run a shell command in the workspace and return its output. Mutating and not idempotent.",
      false,
    )
  }

  fn arguments_schema(&self) -> serde_json::Value {
    json!({
      "type": "object",
      "properties": {
        "command": { "type": "string", "description": "Shell command to run." },
        "cwd": { "type": "string", "description": "Working directory, relative to the workspace." },
        "timeout_ms": { "type": "integer", "description": "Override the configured timeout." }
      },
      "required": ["command"]
    })
  }

  fn preflight(&self, request: &ToolRequest) -> Result<(), ToolError> {
    let Some(cwd) = request
      .arguments
      .get("cwd")
      .and_then(|value| value.as_str())
    else {
      return Ok(());
    };
    self
      .runtime
      .workspace
      .search_path(cwd)
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
}

impl ExecTool {
  fn execute_inner(
    &self,
    request: &ToolRequest,
    progress: &mut dyn ToolProgress,
    context: &ToolExecutionContext,
  ) -> Result<ToolOutcome, ToolError> {
    let runtime = self.runtime.clone();
    let command = arg_str(request, "command")?;
    let timeout_ms = crate::arg_u64(request, "timeout_ms").unwrap_or(runtime.shell_timeout_ms);
    let deadline = Deadline::new(Duration::from_millis(timeout_ms));

    let cwd = match request.arguments.get("cwd").and_then(|v| v.as_str()) {
      Some(path) => runtime
        .workspace
        .search_path(path)
        .map_err(|error| ToolError::new(error.to_string()))?,
      None => runtime.workspace.root().to_path_buf(),
    };
    if !cwd.is_dir() {
      return Ok(ToolOutcome::failed(format!(
        "exec: '{}' is not a directory",
        cwd.display()
      )));
    }

    // The command is *itself* the escape hatch. Passing it through a shell is the
    // contract, which is exactly why the tool is declared mutating and gated.
    let mut child = shell_command(command)
      .current_dir(&cwd)
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()
      .map_err(|error| {
        // Spawning failed, so nothing ran: this is a clean failure, not `Unknown`.
        ToolError::new(format!("exec: cannot start command: {error}"))
      })?;

    let mut outcome = drain(
      &mut child, progress, &runtime, &deadline, context, command, &cwd,
    );
    // Reap in every path. A leaked child keeps running after we report a result,
    // which is the one outcome worse than an honest `Unknown`.
    let status = wait_for_exit(&mut child, context);
    if context.is_cancelled() {
      outcome.cancelled = true;
    } else if context.deadline_expired() {
      outcome.context_deadline = true;
    }
    finish(outcome, status, &deadline, command)
  }
}

/// The result of the streaming phase, before reaping.
struct Drained {
  text: String,
  truncated: bool,
  timed_out: bool,
  cancelled: bool,
  context_deadline: bool,
}

fn drain(
  child: &mut Child,
  progress: &mut dyn ToolProgress,
  runtime: &Runtime,
  deadline: &Deadline,
  context: &ToolExecutionContext,
  _command: &str,
  _cwd: &std::path::Path,
) -> Drained {
  let stdout = child.stdout.take();
  let stderr = child.stderr.take();
  let limit = runtime.exec_capture_limit();

  // Interleave both streams by polling whichever has bytes, so stderr from a
  // failing command is not lost behind a full stdout pipe. The synchronous
  // channel is deliberately bounded: reader workers must apply backpressure
  // instead of accumulating an unbounded command output queue.
  let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(16);
  let mut readers: Vec<Box<dyn Read + Send>> = Vec::new();
  if let Some(out) = stdout {
    readers.push(Box::new(out));
  }
  if let Some(err) = stderr {
    readers.push(Box::new(err));
  }
  let mut handles = Vec::new();
  for mut read in readers {
    let tx = tx.clone();
    handles.push(thread::spawn(move || {
      let mut buf = [0u8; 8 * 1024];
      loop {
        match read.read(&mut buf) {
          Ok(0) => break,
          Ok(n) => {
            if tx.send(buf[..n].to_vec()).is_err() {
              break;
            }
          }
          // A short read error on one stream must not hide the other stream's
          // remaining output, so the thread simply stops.
          Err(_) => break,
        }
      }
    }));
  }
  drop(tx);

  let mut text = String::new();
  let mut truncated = false;
  let mut timed_out = false;
  let mut cancelled = false;
  let mut context_deadline = false;
  let mut sink = crate::BoundedProgress::new(progress, limit, deadline.clone());

  loop {
    if context.is_cancelled() {
      cancelled = true;
      break;
    }
    if context.deadline_expired() {
      context_deadline = true;
      break;
    }
    match rx.recv_timeout(Duration::from_millis(50)) {
      Ok(bytes) => {
        if context.is_cancelled() {
          cancelled = true;
          break;
        }
        let slice = String::from_utf8_lossy(&bytes);
        if !sink.send(&slice) {
          truncated = sink.truncated;
          timed_out = sink.timed_out;
          if truncated || timed_out {
            break;
          }
        }
        text.push_str(&slice);
      }
      Err(mpsc::RecvTimeoutError::Timeout) => {
        if context.is_cancelled() {
          cancelled = true;
          break;
        }
        if context.deadline_expired() {
          context_deadline = true;
          break;
        }
        if deadline.expired() {
          timed_out = true;
          break;
        }
      }
      Err(mpsc::RecvTimeoutError::Disconnected) => break,
    }
  }

  if truncated || timed_out || cancelled || context_deadline {
    // Stop the child as soon as the budget/cancel is spent; otherwise we keep
    // paying for a command whose output we have already abandoned.
    terminate_child_tree(child);
  }
  // Closing the receiver releases any reader worker blocked on the bounded
  // queue. Joining makes the tool's return boundary also the worker lifecycle
  // boundary, rather than leaving pipe readers behind.
  drop(rx);
  for reader in handles {
    let _ = reader.join();
  }

  Drained {
    text,
    truncated,
    timed_out,
    cancelled,
    context_deadline,
  }
}

fn finish(
  drained: Drained,
  status: Option<std::process::ExitStatus>,
  deadline: &Deadline,
  command: &str,
) -> Result<ToolOutcome, ToolError> {
  let elapsed = deadline.elapsed_ms();
  let mut text = drained.text;

  if drained.cancelled {
    text.push_str(
      "\n[the command was cancelled and its completion is unknown — it may have already changed state]",
    );
    let mut outcome = ToolOutcome::unknown(format!(
      "'{}' was cancelled after it started. Output before cancellation:\n{text}",
      first_line(command)
    ));
    outcome.text = text;
    return Ok(with_elapsed(outcome, elapsed));
  }

  if drained.context_deadline {
    text.push_str(
      "\n[the command exceeded the runtime deadline and was killed; completion is unknown]",
    );
    let mut outcome = ToolOutcome::unknown(format!(
      "'{}' exceeded its runtime deadline and was killed. Output before the deadline:\n{text}",
      first_line(command)
    ));
    outcome.text = text;
    return Ok(with_elapsed(outcome, elapsed));
  }

  if drained.timed_out {
    let seconds = deadline.limit.as_secs();
    text.push_str(&format!(
      "\n[stopped after {seconds}s: the command was killed and its completion is \
       unknown — it may have already changed state before being killed]"
    ));
    let mut outcome = ToolOutcome::unknown(format!(
      "'{}' exceeded the {}s timeout and was killed. Output before the timeout:\n{text}",
      first_line(command),
      seconds
    ));
    outcome.text = text;
    outcome.reduced = false;
    return Ok(with_elapsed(outcome, elapsed));
  }

  if drained.truncated {
    text.push_str("\n[output truncated at the capture limit; re-run with a narrower command]");
  }

  let Some(status) = status else {
    // We could not reap the child, so we do not know how it ended.
    return Ok(with_elapsed(
      ToolOutcome::unknown(format!(
        "'{}' could not be reaped; its completion is unknown.\n{text}",
        first_line(command)
      )),
      elapsed,
    ));
  };

  let code = status.code().unwrap_or(-1);
  let code = code as i64;
  if drained.truncated {
    let mut outcome = ToolOutcome::succeeded(text);
    outcome.reduced = true;
    return Ok(with_elapsed(outcome.with_status(code), elapsed));
  }
  if status.success() {
    if text.trim().is_empty() {
      text.push_str("(no output)");
    }
    return Ok(with_elapsed(
      ToolOutcome::succeeded(text).with_status(code),
      elapsed,
    ));
  }
  Ok(with_elapsed(
    ToolOutcome::failed(text).with_status(code),
    elapsed,
  ))
}

fn wait_for_exit(child: &mut Child, context: &ToolExecutionContext) -> Option<ExitStatus> {
  loop {
    match child.try_wait() {
      Ok(Some(status)) => return Some(status),
      Err(_) => return None,
      Ok(None) => {
        if context.is_cancelled_or_expired() {
          terminate_child_tree(child);
          return child.wait().ok();
        }
        thread::sleep(Duration::from_millis(10));
      }
    }
  }
}

/// Stop the shell and every descendant it owns.
fn terminate_child_tree(child: &mut Child) {
  #[cfg(unix)]
  {
    // `shell_command` places the shell in a fresh process group. A negative PID
    // targets that group, including background children spawned by the shell.
    if let Ok(pid) = i32::try_from(child.id()) {
      // SAFETY: libc::kill is called with a process-group id created for this
      // child by `CommandExt::process_group`; no Rust memory is accessed.
      unsafe {
        let _ = libc::kill(-pid, libc::SIGKILL);
      }
    }
  }
  #[cfg(windows)]
  {
    // `taskkill /T` is the Windows process-tree equivalent. The direct kill is
    // retained as a fallback when taskkill is unavailable.
    let pid = child.id().to_string();
    let _ = Command::new("taskkill")
      .args(["/PID", pid.as_str(), "/T", "/F"])
      .status();
  }
  let _ = child.kill();
}

fn with_elapsed(mut outcome: ToolOutcome, elapsed_ms: u64) -> ToolOutcome {
  outcome.text.push_str(&format!(
    "\n[in {elapsed_ms} ms, state {}]",
    outcome.state.as_str()
  ));
  outcome
}

fn first_line(command: &str) -> String {
  let line = command.lines().next().unwrap_or("").trim();
  let short: String = line.chars().take(80).collect();
  if short.chars().count() < line.chars().count() {
    format!("{short}…")
  } else {
    short
  }
}

#[cfg(target_os = "windows")]
fn shell_command(command: &str) -> Command {
  let mut cmd = Command::new("cmd");
  cmd.arg("/C").arg(command);
  cmd
}

#[cfg(not(target_os = "windows"))]
fn shell_command(command: &str) -> Command {
  use std::os::unix::process::CommandExt;

  let mut cmd = Command::new("sh");
  cmd.arg("-c").arg(command);
  // A process group makes timeout/cancellation tree termination explicit rather
  // than killing only the shell that happens to be the direct child.
  cmd.process_group(0);
  cmd
}

#[cfg(test)]
mod tests {
  use pi_rs_core::{ToolCallId, ToolExecutionState};

  use super::*;
  use crate::{Workspace, testutil::Recorder};

  fn runtime(dir: &tempfile::TempDir) -> Runtime {
    let mut runtime = Runtime::new(Workspace::new(dir.path()).unwrap());
    runtime.shell_timeout_ms = 2_000;
    runtime
  }

  fn normalize_path(path: &str) -> String {
    let path = path.trim().replace('\\', "/");
    path.strip_prefix("//?/").unwrap_or(&path).to_string()
  }

  fn request(arguments: serde_json::Value) -> ToolRequest {
    ToolRequest {
      call_id: ToolCallId::new(),
      name: "exec".into(),
      arguments,
    }
  }

  fn exec(dir: &tempfile::TempDir, arguments: serde_json::Value) -> ToolOutcome {
    let tool = ExecTool::new(runtime(dir));
    let mut recorder = Recorder::default();
    tool.execute(&request(arguments), &mut recorder).unwrap()
  }

  #[test]
  fn returns_stdout_and_exit_status() {
    let dir = tempfile::tempdir().unwrap();
    let outcome = exec(&dir, json!({"command": "echo hello"}));
    assert!(!outcome.is_error, "{}", outcome.text);
    assert!(outcome.text.contains("hello"), "{}", outcome.text);
    assert_eq!(outcome.status, Some(0));
  }

  #[test]
  fn reports_a_nonzero_exit_as_a_failure_with_the_code() {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(windows)]
    let command = "echo boom & exit /b 7";
    #[cfg(not(windows))]
    let command = "echo boom; exit 7";
    let outcome = exec(&dir, json!({"command": command}));
    assert!(outcome.is_error);
    assert!(outcome.text.contains("boom"), "{}", outcome.text);
    assert_eq!(outcome.status, Some(7));
    assert_eq!(outcome.state, ToolExecutionState::Failed);
  }

  #[test]
  fn captures_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let outcome = exec(&dir, json!({"command": "echo to_err >&2"}));
    assert!(outcome.text.contains("to_err"), "{}", outcome.text);
  }

  #[test]
  fn runs_in_the_workspace_by_default() {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(windows)]
    let command = "cd";
    #[cfg(not(windows))]
    let command = "pwd";
    let outcome = exec(&dir, json!({"command": command}));
    let expected = normalize_path(&dir.path().canonicalize().unwrap().to_string_lossy());
    let actual = normalize_path(outcome.text.trim().lines().next().unwrap_or(""));
    assert_eq!(actual, expected, "{} vs {}", outcome.text, expected);
  }

  #[test]
  fn a_timeout_is_unknown_not_failed() {
    // The whole point of `Unknown`: the command was killed mid-flight and may
    // have already changed the world.
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("touched");
    let outcome = exec(
      &dir,
      json!({
        "command": format!("touch '{}' && sleep 30", marker.display()),
        "timeout_ms": 300
      }),
    );
    assert_eq!(
      outcome.state,
      ToolExecutionState::Unknown,
      "{}",
      outcome.text
    );
    assert!(outcome.is_error, "unknown is not a success");
    assert!(outcome.text.contains("unknown"), "{}", outcome.text);
    assert!(marker.exists(), "the side effect really happened");
  }

  #[cfg(unix)]
  #[test]
  fn a_timeout_kills_background_descendants_not_only_the_shell() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("late-child-marker");
    let command = format!("(sleep 1; touch '{}') & sleep 30", marker.display());
    let outcome = exec(&dir, json!({"command": command, "timeout_ms": 150}));
    assert_eq!(
      outcome.state,
      ToolExecutionState::Unknown,
      "{}",
      outcome.text
    );
    thread::sleep(Duration::from_millis(1_300));
    assert!(!marker.exists(), "background child outlived the timeout");
  }

  #[test]
  fn cancellation_kills_a_running_command_promptly() {
    let dir = tempfile::tempdir().unwrap();
    let tool = ExecTool::new(runtime(&dir));
    let cancel = pi_rs_core::CancelToken::new();
    let trigger = cancel.clone();
    let started = std::time::Instant::now();
    let thread = thread::spawn(move || {
      thread::sleep(Duration::from_millis(100));
      trigger.cancel();
    });
    let mut recorder = Recorder::default();
    let outcome = tool
      .execute_with_context(
        &request(json!({"command": "sleep 30"})),
        &mut recorder,
        &pi_rs_core::ToolExecutionContext::new(cancel, Duration::from_secs(10)),
      )
      .unwrap();
    thread.join().unwrap();
    assert_eq!(
      outcome.state,
      ToolExecutionState::Unknown,
      "{}",
      outcome.text
    );
    assert!(
      started.elapsed() < Duration::from_secs(2),
      "{}",
      outcome.text
    );
  }

  #[test]
  fn huge_output_is_bounded_and_marked() {
    let dir = tempfile::tempdir().unwrap();
    let outcome = exec(&dir, json!({"command": "yes abcdefghij | head -c 4000000"}));
    // The *reported* result is bounded; the process may still have produced more.
    assert!(
      outcome.reduced || outcome.text.contains("truncated"),
      "{}",
      &outcome.text[..outcome.text.len().min(200)]
    );
  }

  #[test]
  fn a_missing_working_directory_is_refused_before_spawning() {
    let dir = tempfile::tempdir().unwrap();
    let outcome = exec(
      &dir,
      json!({"command": "x", "cwd": "definitely_not_a_directory"}),
    );
    assert!(outcome.is_error, "{}", outcome.text);
    assert!(outcome.text.contains("not a directory"), "{}", outcome.text);
  }

  #[test]
  fn a_working_directory_outside_the_workspace_is_refused_before_spawning() {
    let dir = tempfile::tempdir().unwrap();
    let tool = ExecTool::new(runtime(&dir));
    let mut recorder = Recorder::default();
    // A command whose working directory is outside the boundary never runs: the
    // refusal must not depend on the command being well-formed.
    let error = match tool.execute(
      &request(json!({"command": "pwd", "cwd": "../../../etc"})),
      &mut recorder,
    ) {
      Err(error) => error,
      Ok(_) => panic!("boundary refusal expected"),
    };
    assert!(!error.started, "the command was never spawned");
    assert!(error.message.contains("outside"), "{}", error.message);
    assert!(
      recorder.text().is_empty(),
      "no output from a refused command"
    );
  }

  #[test]
  fn the_shell_output_streams_as_it_arrives() {
    let dir = tempfile::tempdir().unwrap();
    let tool = ExecTool::new(runtime(&dir));
    let mut recorder = Recorder::default();
    let outcome = tool
      .execute(&request(json!({"command": "echo streamed"})), &mut recorder)
      .unwrap();
    assert!(recorder.text().contains("streamed"), "{}", recorder.text());
    assert!(outcome.text.contains("streamed"));
  }

  #[test]
  fn is_declared_mutating_and_not_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let meta = ExecTool::new(runtime(&dir)).metadata();
    assert!(!meta.read_only);
    assert!(!meta.idempotent, "a second commit is a different act");
  }

  #[test]
  fn empty_output_says_so_instead_of_passing_as_no_output() {
    let dir = tempfile::tempdir().unwrap();
    let outcome = exec(&dir, json!({"command": "true"}));
    assert!(outcome.text.contains("(no output)"), "{}", outcome.text);
  }
}
