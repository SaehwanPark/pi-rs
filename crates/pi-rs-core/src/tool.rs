//! Tool lifecycle contract.
//!
//! Every tool invocation has a durable id and a durable lifecycle state. That
//! is not bookkeeping for its own sake: it is what makes it possible to answer
//! "may this side effect run again?" after a crash, a stream interruption, or a
//! model failover.
//!
//! [`ToolExecutionState::Unknown`] is a terminal state in its own right. It is
//! never coerced into success or failure, and a mutating call in `Unknown` is
//! never blindly replayed.

use serde::{Deserialize, Serialize};

use crate::{
  ids::ToolCallId,
  message::{ContentBlock, ToolResultBlock},
  trace::BlobRef,
};

/// Durable lifecycle state of one tool invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionState {
  /// The model asked for the call and the runtime recorded it.
  Requested,
  /// Execution began.
  Started,
  /// Completion observed with a successful result.
  Succeeded,
  /// Completion observed with a failure result.
  Failed,
  /// Completion could not be observed. The side effect may or may not have
  /// happened.
  Unknown,
}

impl ToolExecutionState {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Requested => "requested",
      Self::Started => "started",
      Self::Succeeded => "succeeded",
      Self::Failed => "failed",
      Self::Unknown => "unknown",
    }
  }

  /// `true` for states that ended execution.
  pub fn is_terminal(self) -> bool {
    matches!(self, Self::Succeeded | Self::Failed | Self::Unknown)
  }

  /// `true` when a result exists that later stages must reuse rather than
  /// regenerate.
  pub fn has_committed_result(self) -> bool {
    matches!(self, Self::Succeeded | Self::Failed)
  }

  /// `true` when the outside world may have changed in a way this session did
  /// not observe.
  pub fn side_effect_uncertain(self) -> bool {
    matches!(self, Self::Started | Self::Unknown)
  }

  /// Whether the call may be executed again after an interruption.
  pub fn replay_decision(self, metadata: &ToolMetadata) -> ReplayDecision {
    match self {
      // A committed result is part of canonical history; reuse it.
      Self::Succeeded => ReplayDecision::Never,
      // Observed failure. Uncertainty must be recorded as `Unknown`, so
      // retrying an observed failure is allowed.
      Self::Failed => ReplayDecision::Replay,
      Self::Requested | Self::Started | Self::Unknown if metadata.read_only => {
        ReplayDecision::Replay
      }
      Self::Requested | Self::Started | Self::Unknown => ReplayDecision::ReconcileFirst,
    }
  }
}

/// What the runtime may do with a call that was interrupted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayDecision {
  /// Safe to execute again with no user involvement.
  Replay,
  /// The runtime must reconcile first: inspect current state, and ask the user
  /// before repeating the effect.
  ReconcileFirst,
  /// Must not execute again; reuse the committed result.
  Never,
}

/// Static description of a tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolMetadata {
  pub name: String,
  pub description: String,
  /// `true` when execution cannot change observable state outside the runtime.
  pub read_only: bool,
  /// `true` when repeating the identical call is guaranteed to converge to the
  /// same external state.
  pub idempotent: bool,
}

impl ToolMetadata {
  pub fn read_only(name: impl Into<String>, description: impl Into<String>) -> Self {
    Self {
      name: name.into(),
      description: description.into(),
      read_only: true,
      idempotent: true,
    }
  }

  pub fn mutating(
    name: impl Into<String>,
    description: impl Into<String>,
    idempotent: bool,
  ) -> Self {
    Self {
      name: name.into(),
      description: description.into(),
      read_only: false,
      idempotent,
    }
  }
}

/// One requested invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRequest {
  pub call_id: ToolCallId,
  pub name: String,
  pub arguments: serde_json::Value,
}

/// Incremental output while a tool runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolChunk {
  pub text: String,
}

impl ToolChunk {
  pub fn new(text: impl Into<String>) -> Self {
    Self { text: text.into() }
  }
}

/// Result of one execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutcome {
  pub state: ToolExecutionState,
  /// Model-visible text. May be a bounded representation when `blob` is set or
  /// when `reduced` is true.
  pub text: String,
  #[serde(default)]
  pub is_error: bool,
  /// `true` when the model-visible text is smaller than the real output.
  #[serde(default)]
  pub reduced: bool,
  /// Reference to the full payload in the trace store.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub blob: Option<BlobRef>,
  /// Exit-like status where the tool has one, kept as a value rather than a
  /// stringly-typed convention.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub status: Option<i64>,
}

impl ToolOutcome {
  pub fn succeeded(text: impl Into<String>) -> Self {
    Self {
      state: ToolExecutionState::Succeeded,
      text: text.into(),
      is_error: false,
      reduced: false,
      blob: None,
      status: None,
    }
  }

  pub fn failed(text: impl Into<String>) -> Self {
    Self {
      state: ToolExecutionState::Failed,
      text: text.into(),
      is_error: true,
      reduced: false,
      blob: None,
      status: None,
    }
  }

  /// Record an execution whose completion was not observed.
  ///
  /// The text must say what is uncertain, because it enters model context and
  /// may be the only description the next model sees.
  pub fn unknown(text: impl Into<String>) -> Self {
    Self {
      state: ToolExecutionState::Unknown,
      text: text.into(),
      is_error: true,
      reduced: false,
      blob: None,
      status: None,
    }
  }

  pub fn with_blob(mut self, blob: BlobRef) -> Self {
    self.blob = Some(blob);
    self.reduced = true;
    self
  }

  pub fn with_status(mut self, status: i64) -> Self {
    self.status = Some(status);
    self
  }

  pub fn to_block(&self, call_id: ToolCallId, name: &str) -> ToolResultBlock {
    ToolResultBlock {
      id: call_id,
      name: name.to_string(),
      state: self.state,
      text: self.text.clone(),
      is_error: self.is_error,
      reduced: self.reduced,
    }
  }

  pub fn into_block(self, call_id: ToolCallId, name: &str) -> ContentBlock {
    ContentBlock::ToolResult(self.to_block(call_id, name))
  }
}

/// Infrastructure error from the tool runtime itself, as opposed to a tool
/// reporting a failure in its outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolError {
  /// What went wrong in harness terms, for example "unknown tool" or "invalid
  /// arguments".
  pub message: String,
  /// `true` when the failure happened after execution may have begun, which
  /// forces [`ToolExecutionState::Unknown`] for mutating tools instead of
  /// `Failed`.
  pub started: bool,
}

impl ToolError {
  pub fn new(message: impl Into<String>) -> Self {
    Self {
      message: message.into(),
      started: false,
    }
  }

  pub fn after_start(message: impl Into<String>) -> Self {
    Self {
      message: message.into(),
      started: true,
    }
  }

  /// The lifecycle state implied by this error for a given tool.
  pub fn implied_state(&self, metadata: &ToolMetadata) -> ToolExecutionState {
    if self.started && !metadata.read_only {
      ToolExecutionState::Unknown
    } else {
      ToolExecutionState::Failed
    }
  }
}

impl std::fmt::Display for ToolError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(&self.message)
  }
}

/// Progress sink passed to a running tool.
///
/// Tools stream through this rather than writing to stdout. Progress chunks are
/// transient surface output; durable history records the final bounded tool
/// result under its lifecycle event.
pub trait ToolProgress: Send {
  fn emit(&mut self, chunk: &ToolChunk);
}

impl<F> ToolProgress for F
where
  F: FnMut(&ToolChunk) + Send,
{
  fn emit(&mut self, chunk: &ToolChunk) {
    self(chunk)
  }
}

/// One executable tool.
pub trait Tool: Send + Sync {
  fn metadata(&self) -> ToolMetadata;

  /// JSON Schema for arguments, handed to tool-capable providers.
  fn arguments_schema(&self) -> serde_json::Value;

  /// Validate the call at the pre-execution boundary.
  ///
  /// This hook is for tool-specific checks, such as workspace confinement, that
  /// prove the tool must not run. The registry invokes it before emitting
  /// `ToolStarted`; implementations must not perform the operation itself.
  /// The default keeps existing extension tools source-compatible.
  fn preflight(&self, _request: &ToolRequest) -> Result<(), ToolError> {
    Ok(())
  }

  /// Execute the call. Implementations must return
  /// [`ToolOutcome::unknown`] rather than `failed` when completion cannot be
  /// observed, and must not swallow cancellation into a success.
  fn execute(
    &self,
    request: &ToolRequest,
    progress: &mut dyn ToolProgress,
  ) -> Result<ToolOutcome, ToolError>;
}

#[cfg(test)]
mod tests {
  use super::*;

  fn reader() -> ToolMetadata {
    ToolMetadata::read_only("read", "read a file")
  }

  fn writer() -> ToolMetadata {
    ToolMetadata::mutating("write", "write a file", false)
  }

  #[test]
  fn read_only_calls_may_be_retried_from_any_uncommitted_state() {
    for state in [
      ToolExecutionState::Requested,
      ToolExecutionState::Started,
      ToolExecutionState::Unknown,
      ToolExecutionState::Failed,
    ] {
      assert_eq!(
        state.replay_decision(&reader()),
        ReplayDecision::Replay,
        "{}",
        state.as_str()
      );
    }
  }

  #[test]
  fn mutating_calls_are_never_blindly_replayed() {
    for state in [
      ToolExecutionState::Requested,
      ToolExecutionState::Started,
      ToolExecutionState::Unknown,
    ] {
      assert_eq!(
        state.replay_decision(&writer()),
        ReplayDecision::ReconcileFirst,
        "{}",
        state.as_str()
      );
    }
    assert_eq!(
      ToolExecutionState::Succeeded.replay_decision(&writer()),
      ReplayDecision::Never
    );
  }

  #[test]
  fn uncertainty_is_distinct_from_failure() {
    let unknown = ToolOutcome::unknown("write issued, completion not observed");
    let failed = ToolOutcome::failed("write rejected: permission denied");
    assert_eq!(unknown.state, ToolExecutionState::Unknown);
    assert_eq!(failed.state, ToolExecutionState::Failed);
    assert!(unknown.state.side_effect_uncertain());
    assert!(!failed.state.side_effect_uncertain());
    assert!(failed.state.has_committed_result());
    assert!(!unknown.state.has_committed_result());
  }

  #[test]
  fn after_start_error_on_mutating_tool_is_unknown() {
    let error = ToolError::after_start("process killed before exit status was read");
    assert_eq!(
      error.implied_state(&writer()),
      ToolExecutionState::Unknown,
      "a mutating call whose end was not observed must not be reported as failed"
    );
    assert_eq!(
      error.implied_state(&reader()),
      ToolExecutionState::Failed,
      "a read-only call can be reported as failed safely"
    );
  }
}
