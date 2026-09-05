//! The tool registry: the set, the policy, and the lifecycle.
//!
//! The registry is the only place that knows the whole tool set, and it is the
//! only place that decides whether a call may run. That concentration is the
//! point: policy gating, approval, argument validation, output reduction, and
//! lifecycle recording all live here so a tool implementation cannot accidentally
//! become its own authority.
//!
//! The registry produces an [`Executed`] record rather than mutating session
//! state itself. The runtime owns durability; the registry owns decisions.

use std::collections::BTreeMap;

use pi_rs_core::{
  CancelToken, ReplayDecision, Tool, ToolChunk, ToolExecutionState, ToolMetadata, ToolOutcome,
  ToolProgress, ToolRequest,
};
use serde_json::Value;

use crate::{Deadline, Runtime, paths::Workspace, reduce};

/// Whether a call is cleared to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approval {
  /// Run it.
  Allow,
  /// Refuse, with model-facing reasoning.
  Deny(String),
  /// Ask a human. The *surface* renders the prompt; the registry never blocks
  /// waiting for input, because a blocked tool thread cannot be cancelled cleanly
  /// and would report `Unknown` for a call that never ran.
  ///
  /// A surface that cannot reach a human must not return this variant: an
  /// unanswered question would otherwise read as permission. That is why the
  /// default gate denies, and why "ask" is only produced by a real approver.
  Ask(String),
}

/// How the registry answers a mutating call when the caller supplies no gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DefaultGate {
  /// Mutation needs approval, and nothing here can grant it.
  Deny,
  /// `tool.auto_approve_mutating` granted it in advance.
  Allow,
}

/// The gate for mutating tools.
///
/// An approval gate, not an approval prompt: keeping the decision external is
/// what lets the same registry run headless, under a TUI, or in a test.
pub trait ApprovalGate {
  fn decide(&mut self, metadata: &ToolMetadata, arguments: &Value) -> Approval;
}

/// The closed default when the policy has not granted mutation: nothing may
/// change state until a surface that can actually ask attaches its own gate.
///
/// This is the behaviour that must never regress in a headless run. A prompt no
/// one answers is not a safety control; a refusal the model can read is.
pub struct DenyMutating;

impl ApprovalGate for DenyMutating {
  fn decide(&mut self, metadata: &ToolMetadata, _arguments: &Value) -> Approval {
    Approval::Deny(format!(
      "'{name}' is mutating and approval is required. Nothing was changed. Grant        this tool by name under tool.allow_mutating, or run a surface that can ask.",
      name = metadata.name
    ))
  }
}

/// Approve every mutating call.
///
/// Only reachable through `tool.auto_approve_mutating`, i.e. an operator who has
/// already accepted the risk by configuration, not by silence.
pub struct AutoApprove;

impl ApprovalGate for AutoApprove {
  fn decide(&mut self, _metadata: &ToolMetadata, _arguments: &Value) -> Approval {
    Approval::Allow
  }
}

/// Refuse every mutating call.
pub struct DenyAll;

impl ApprovalGate for DenyAll {
  fn decide(&mut self, metadata: &ToolMetadata, _arguments: &Value) -> Approval {
    Approval::Deny(format!(
      "'{}' changes state and mutating tools are not approved by policy",
      metadata.name
    ))
  }
}

/// The result of one registry-mediated execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Executed {
  /// The request that was executed, kept so the record is self-describing.
  pub request: ToolRequest,
  /// Outcome as returned by the tool, before lifecycle coercion.
  pub outcome: ToolOutcome,
  /// Lifecycle actually recorded. Differs from the outcome's state when the
  /// runtime had to coerce it — a mutating tool that reported success after
  /// cancellation is the important case.
  pub state: ToolExecutionState,
  /// `true` when the tool ran at all.
  pub started: bool,
  /// Why the call was refused, when it was.
  pub refusal: Option<String>,
  /// Full output bytes when the model-visible text was reduced.
  pub full_output: Option<Vec<u8>>,
  /// `true` when cancellation was observed.
  pub cancelled: bool,
}

impl Executed {
  /// A refusal that never started execution.
  pub(crate) fn refused(request: ToolRequest, reason: impl Into<String>) -> Self {
    let reason = reason.into();
    Self {
      request,
      outcome: ToolOutcome::failed(reason.clone()),
      state: ToolExecutionState::Failed,
      started: false,
      refusal: Some(reason),
      full_output: None,
      cancelled: false,
    }
  }

  /// The block to append to context for this call.
  pub fn to_block(&self) -> pi_rs_core::ToolResultBlock {
    self
      .outcome
      .to_block(self.request.call_id.clone(), &self.request.name)
  }

  /// Whether the call may be executed again after an interruption.
  pub fn replay_decision(&self, metadata: &ToolMetadata) -> ReplayDecision {
    self.state.replay_decision(metadata)
  }
}

/// The set of tools the runtime provides.
pub struct ToolRegistry {
  runtime: Runtime,
  tools: BTreeMap<String, Box<dyn Tool>>,
  allow: Vec<String>,
  deny: Vec<String>,
  /// How a mutating call is answered when the caller does not supply a gate.
  ///
  /// Stored as data, not a trait object, so answering never requires borrowing the
  /// registry mutably while it is also being read.
  default_gate: DefaultGate,
}

impl ToolRegistry {
  /// An empty registry over a workspace.
  pub fn new(workspace: Workspace) -> Self {
    Self {
      runtime: Runtime::new(workspace),
      tools: BTreeMap::new(),
      allow: Vec::new(),
      deny: Vec::new(),
      default_gate: DefaultGate::Deny,
    }
  }

  /// Apply the configured tool policy.
  pub fn with_policy(mut self, policy: &pi_rs_core::ToolPolicy) -> Self {
    self.runtime = self.runtime.with_policy(policy);
    self.allow = policy.allow.clone();
    self.deny = policy.deny.clone();
    // The operator's standing answer, expressed as data. Unset stays closed.
    self.default_gate = if policy.auto_approve_mutating {
      DefaultGate::Allow
    } else {
      DefaultGate::Deny
    };
    self
  }

  /// Register a tool, replacing any tool with the same name.
  ///
  /// Replacement is allowed on purpose: it is how an extension overrides a
  /// built-in without the runtime needing a second resolution rule.
  pub fn register(&mut self, tool: Box<dyn Tool>) -> &mut Self {
    let name = tool.metadata().name;
    self.tools.insert(name, tool);
    self
  }

  /// Register the built-in set.
  ///
  /// The built-in set is the whole point of registering tools instead of
  /// hard-coding a dispatch: adding a tool never touches the registry.
  pub fn with_builtins(mut self) -> Self {
    self.register(Box::new(crate::ReadTool::new(self.runtime.clone())));
    self.register(Box::new(crate::WriteTool::new(self.runtime.clone())));
    self.register(Box::new(crate::GrepTool::new(self.runtime.clone())));
    self.register(Box::new(crate::EditTool::new(self.runtime.clone())));
    self.register(Box::new(crate::ExecTool::new(self.runtime.clone())));
    self
  }

  pub fn len(&self) -> usize {
    self.tools.len()
  }

  pub fn is_empty(&self) -> bool {
    self.tools.is_empty()
  }

  pub fn names(&self) -> Vec<String> {
    self.tools.keys().cloned().collect()
  }

  /// Whether the policy lets this tool be offered and called at all.
  pub fn is_allowed(&self, name: &str) -> bool {
    if self.deny.iter().any(|d| d == name) {
      return false;
    }
    if self.allow.is_empty() {
      return true;
    }
    self.allow.iter().any(|a| a == name)
  }

  /// Names currently permitted by policy.
  pub fn allowed_names(&self) -> Vec<String> {
    self
      .tools
      .keys()
      .filter(|name| self.is_allowed(name))
      .cloned()
      .collect()
  }

  /// Metadata for permitted tools, in registration order.
  pub fn specs(&self) -> Vec<pi_rs_core::ToolSpec> {
    self
      .tools
      .iter()
      .filter(|(name, _)| self.is_allowed(name))
      .map(|(_, tool)| spec_of(tool.as_ref()))
      .collect()
  }

  /// Metadata for permitted tools, for capability reporting and prompts.
  pub fn metadata(&self) -> Vec<ToolMetadata> {
    self
      .tools
      .iter()
      .filter(|(name, _)| self.is_allowed(name))
      .map(|(_, tool)| tool.metadata())
      .collect()
  }

  pub fn metadata_for(&self, name: &str) -> Option<ToolMetadata> {
    self.tools.get(name).map(|tool| tool.metadata())
  }

  /// Whether any permitted tool can change state.
  pub fn has_mutating_tools(&self) -> bool {
    self
      .tools
      .iter()
      .any(|(name, tool)| self.is_allowed(name) && !tool.metadata().read_only)
  }

  /// Execute one call under the policy.
  ///
  /// `progress` receives the tool's streamed output. `cancel` is checked before
  /// starting and honoured by tools that observe it; the registry itself never
  /// needs to observe it, because the turn loop stops issuing calls.
  pub fn execute(
    &self,
    request: &ToolRequest,
    progress: &mut dyn ToolProgress,
    cancel: &CancelToken,
  ) -> Executed {
    match self.default_gate {
      DefaultGate::Allow => {
        let mut approve = AutoApprove;
        self.dispatch(request, progress, cancel, &mut approve)
      }
      DefaultGate::Deny => {
        let mut refuse = DenyMutating;
        self.dispatch(request, progress, cancel, &mut refuse)
      }
    }
  }

  /// Execute one call, answering a mutating call with `gate` instead of the
  /// configured default.
  ///
  /// Used by interactive sessions, where a human answers the question, and by
  /// tests, where the answer is asserted rather than typed.
  pub fn execute_with(
    &self,
    request: &ToolRequest,
    progress: &mut dyn ToolProgress,
    cancel: &CancelToken,
    gate: &mut dyn ApprovalGate,
  ) -> Executed {
    self.dispatch(request, progress, cancel, gate)
  }

  /// The single execution path: policy, validation, approval, bounds, reduction,
  /// and lifecycle coercion.
  ///
  /// Every public entry point converges here. They used to duplicate this body,
  /// and the copies drifted until an explicit approval was silently overridden by
  /// the default gate; one path is what keeps that from happening again.
  fn dispatch(
    &self,
    request: &ToolRequest,
    progress: &mut dyn ToolProgress,
    cancel: &CancelToken,
    gate: &mut dyn ApprovalGate,
  ) -> Executed {
    if cancel.is_cancelled() {
      return Executed {
        request: request.clone(),
        outcome: ToolOutcome::failed(format!(
          "'{}' not executed: the turn was cancelled before it started",
          request.name
        )),
        state: ToolExecutionState::Requested,
        started: false,
        refusal: Some("cancelled before execution".to_string()),
        full_output: None,
        cancelled: true,
      };
    }
    let Some(tool) = self.tools.get(&request.name) else {
      return Executed::refused(
        request.clone(),
        unknown_tool(&request.name, &self.allowed_names()),
      );
    };
    let metadata = tool.metadata();
    if !self.is_allowed(&metadata.name) {
      return Executed::refused(
        request.clone(),
        format!("tool '{}' is denied by policy", metadata.name),
      );
    }
    if let Err(message) =
      validate_arguments(&metadata, &request.arguments, &tool.arguments_schema())
    {
      return Executed::refused(request.clone(), message);
    }
    if !metadata.read_only {
      // `Ask` refuses here as well: nothing in this type can deliver a prompt, so
      // an unanswered question must not read as permission. A surface that can ask
      // returns `Allow`/`Deny` after it has actually asked.
      if let Approval::Deny(reason) | Approval::Ask(reason) =
        gate.decide(&metadata, &request.arguments)
      {
        return Executed::refused(request.clone(), reason);
      }
    }

    let mut sink = Sink {
      inner: progress,
      forwarded: false,
    };
    let result = tool.execute(request, &mut sink);

    match result {
      Ok(mut outcome) => {
        let mut full_output = None;
        if let Some(reduction) = reduce::reduce(&outcome.text, self.runtime.max_output_bytes) {
          outcome.text = reduction.text;
          outcome.reduced = true;
          full_output = Some(reduction.full.into_bytes());
        }
        let state = coerce_state(outcome.state, &metadata, cancel.is_cancelled());
        if state != outcome.state {
          outcome.state = state;
        }
        Executed {
          request: request.clone(),
          outcome,
          state,
          started: true,
          refusal: None,
          full_output,
          cancelled: cancel.is_cancelled(),
        }
      }
      Err(error) => {
        // A harness-level error still has to land in a lifecycle state, because
        // the next model must be able to tell whether the world changed.
        let mut state = error.implied_state(&metadata);
        if cancel.is_cancelled() && state == ToolExecutionState::Started {
          state = ToolExecutionState::Unknown;
        }
        let outcome = ToolOutcome {
          state,
          text: error.message.clone(),
          is_error: true,
          reduced: false,
          blob: None,
          status: None,
        };
        Executed {
          request: request.clone(),
          outcome,
          state,
          started: error.started,
          refusal: Some(error.message),
          full_output: None,
          cancelled: cancel.is_cancelled(),
        }
      }
    }
  }
}

/// Progress sink that stops forwarding once a bound is spent.
///
/// The bound exists so a runaway tool cannot make the UI buffer unbounded text
/// that the registry is going to reduce anyway. The tool's own returned text
/// remains the authoritative result; this only gates the live stream.
struct Sink<'a> {
  inner: &'a mut dyn ToolProgress,
  forwarded: bool,
}

impl ToolProgress for Sink<'_> {
  fn emit(&mut self, chunk: &ToolChunk) {
    self.forwarded = true;
    self.inner.emit(chunk);
  }
}

/// Coerce a tool's claimed state into one the runtime can defend.
///
/// Two coercions matter. A mutating tool that claims success while cancellation
/// was observed is not believed: the write may have been half-applied. A tool
/// that claims `Requested` or `Started` as terminal is wrong by contract, so it
/// becomes `Unknown`.
fn coerce_state(
  claimed: ToolExecutionState,
  metadata: &ToolMetadata,
  cancelled: bool,
) -> ToolExecutionState {
  match claimed {
    ToolExecutionState::Requested | ToolExecutionState::Started => ToolExecutionState::Unknown,
    ToolExecutionState::Succeeded if cancelled && !metadata.read_only => {
      ToolExecutionState::Unknown
    }
    other => other,
  }
}

fn spec_of(tool: &dyn Tool) -> pi_rs_core::ToolSpec {
  let metadata = tool.metadata();
  pi_rs_core::ToolSpec {
    name: metadata.name,
    description: metadata.description,
    parameters: tool.arguments_schema(),
  }
}

fn unknown_tool(name: &str, available: &[String]) -> String {
  format!(
    "unknown tool '{name}'; available: {}",
    if available.is_empty() {
      "none".to_string()
    } else {
      available.join(", ")
    }
  )
}

/// Cheap argument validation, so a malformed call costs nothing.
///
/// The provider already saw the schema; this only rejects the shapes that would
/// make a tool panic or silently do nothing, because those are the cases where
/// an early, clear refusal beats a tool's own error text.
fn validate_arguments(
  metadata: &ToolMetadata,
  arguments: &Value,
  schema: &Value,
) -> Result<(), String> {
  let Value::Object(map) = arguments else {
    return Err(format!(
      "'{}' arguments must be a JSON object",
      metadata.name
    ));
  };
  let Some(required) = schema.get("required").and_then(|v| v.as_array()) else {
    return Ok(());
  };
  let properties = schema.get("properties").and_then(|v| v.as_object());
  for key in required {
    let Some(name) = key.as_str() else { continue };
    let Some(value) = map.get(name) else {
      return Err(format!(
        "'{}' is missing required argument '{name}'",
        metadata.name
      ));
    };
    if let Some(kind) = properties
      .and_then(|p| p.get(name))
      .and_then(|p| p.get("type"))
      .and_then(|t| t.as_str())
    {
      let matches = match kind {
        "string" => value.is_string(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => true,
      };
      if !matches {
        return Err(format!(
          "'{}' argument '{name}' must be {kind}",
          metadata.name
        ));
      }
    }
  }
  Ok(())
}

/// A deadline from the configured shell timeout, for tools that budget their own
/// work without going through the registry.
pub fn deadline_for(policy_timeout_ms: u64) -> Deadline {
  Deadline::new(std::time::Duration::from_millis(policy_timeout_ms))
}
