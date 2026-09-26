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
use std::sync::{Arc, RwLock};

use rupi_core::{
  CancelToken, ReplayDecision, Tool, ToolChunk, ToolError, ToolExecutionContext,
  ToolExecutionState, ToolMetadata, ToolOutcome, ToolProgress, ToolRequest,
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
      "'{name}' is mutating and approval is required. Nothing was changed. Grant this tool by name under tool.allow_mutating, or run a surface that can ask.",
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
  pub fn to_block(&self) -> rupi_core::ToolResultBlock {
    self
      .outcome
      .to_block(self.request.call_id.clone(), &self.request.name)
  }

  /// Whether the call may be executed again after an interruption.
  pub fn replay_decision(&self, metadata: &ToolMetadata) -> ReplayDecision {
    self.state.replay_decision(metadata)
  }
}

struct RegisteredTool {
  tool: Arc<dyn Tool>,
  metadata: ToolMetadata,
  schema: Arc<Value>,
}

impl RegisteredTool {
  fn new(tool: Arc<dyn Tool>) -> Self {
    let metadata = tool.metadata();
    let schema = Arc::new(tool.arguments_schema());
    Self {
      tool,
      metadata,
      schema,
    }
  }

  fn spec(&self) -> rupi_core::ToolSpec {
    rupi_core::ToolSpec {
      name: self.metadata.name.clone(),
      description: self.metadata.description.clone(),
      parameters: self.schema.as_ref().clone(),
    }
  }
}

/// The set of tools the runtime provides.
pub struct ToolRegistry {
  runtime: Runtime,
  tools: RwLock<BTreeMap<String, RegisteredTool>>,
  allow: Vec<String>,
  deny: Vec<String>,
  /// How a mutating call is answered when the caller does not supply a gate.
  ///
  /// Stored as data, not a trait object, so answering never requires borrowing the
  /// registry mutably while it is also being read.
  default_gate: DefaultGate,
  /// Backward-compatible `with_policy` cannot return a `Result`; retain an
  /// explicit configuration failure so it refuses execution instead of silently
  /// using the previous workspace.
  configuration_error: Option<String>,
}

impl ToolRegistry {
  /// An empty registry over a workspace.
  pub fn new(workspace: Workspace) -> Self {
    Self {
      runtime: Runtime::new(workspace),
      tools: RwLock::new(BTreeMap::new()),
      allow: Vec::new(),
      deny: Vec::new(),
      default_gate: DefaultGate::Deny,
      configuration_error: None,
    }
  }

  /// Apply the configured tool policy, rejecting an invalid workspace override.
  pub fn try_with_policy(mut self, policy: &rupi_core::ToolPolicy) -> Result<Self, ToolError> {
    self.runtime = self.runtime.try_with_policy(policy)?;
    self.configuration_error = None;
    self.allow = policy.allow.clone();
    self.deny = policy.deny.clone();
    // The operator's standing answer, expressed as data. Unset stays closed.
    self.default_gate = if policy.auto_approve_mutating {
      DefaultGate::Allow
    } else {
      DefaultGate::Deny
    };
    Ok(self)
  }

  /// Backward-compatible infallible policy setter. New callers should use
  /// [`Self::try_with_policy`] so an invalid configured cwd cannot be hidden.
  pub fn with_policy(mut self, policy: &rupi_core::ToolPolicy) -> Self {
    match self.runtime.clone().try_with_policy(policy) {
      Ok(runtime) => {
        self.runtime = runtime;
        self.configuration_error = None;
      }
      Err(error) => {
        self.configuration_error = Some(error.message);
      }
    }
    self.allow = policy.allow.clone();
    self.deny = policy.deny.clone();
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
    let tool: Arc<dyn Tool> = tool.into();
    let registered = RegisteredTool::new(tool);
    let name = registered.metadata.name.clone();
    self.tools.write().unwrap().insert(name, registered);
    self
  }

  /// Register a tool into the registry via a shared reference.
  ///
  /// This enables dynamic mid-session tool registration (e.g. on-demand MCP activation)
  /// without requiring exclusive ownership of the registry.
  pub fn register_shared(&self, tool: Box<dyn Tool>) {
    let tool: Arc<dyn Tool> = tool.into();
    let registered = RegisteredTool::new(tool);
    let name = registered.metadata.name.clone();
    self.tools.write().unwrap().insert(name, registered);
  }

  /// Unregister a tool by name via a shared reference.
  pub fn unregister_shared(&self, name: &str) -> bool {
    self.tools.write().unwrap().remove(name).is_some()
  }

  /// Unregister all tools whose names start with the given prefix.
  pub fn unregister_prefix(&self, prefix: &str) -> usize {
    let mut tools = self.tools.write().unwrap();
    let matching: Vec<String> = tools
      .keys()
      .filter(|name| name.starts_with(prefix))
      .cloned()
      .collect();
    let count = matching.len();
    for name in matching {
      tools.remove(&name);
    }
    count
  }

  /// Whether policy explicitly permits mutating tools to run without a prompt.
  pub fn auto_approves_mutating(&self) -> bool {
    self.default_gate == DefaultGate::Allow
  }

  /// Register the built-in set.
  ///
  /// The built-in set is the whole point of registering tools instead of
  /// hard-coding a dispatch: adding a tool never touches the registry.
  pub fn with_builtins(mut self) -> Self {
    self.register(Box::new(crate::ReadTool::new(self.runtime.clone())));
    self.register(Box::new(crate::WriteTool::new(self.runtime.clone())));
    self.register(Box::new(crate::AppendTool::new(self.runtime.clone())));
    self.register(Box::new(crate::GrepTool::new(self.runtime.clone())));
    self.register(Box::new(crate::EditTool::new(self.runtime.clone())));
    self.register(Box::new(crate::ExecTool::new(self.runtime.clone())));
    self.register(Box::new(crate::ProcessTool::new(self.runtime.clone())));
    self
  }

  pub fn len(&self) -> usize {
    self.tools.read().unwrap().len()
  }

  pub fn is_empty(&self) -> bool {
    self.tools.read().unwrap().is_empty()
  }

  pub fn names(&self) -> Vec<String> {
    self.tools.read().unwrap().keys().cloned().collect()
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
      .read()
      .unwrap()
      .keys()
      .filter(|name| self.is_allowed(name))
      .cloned()
      .collect()
  }

  /// Metadata for permitted tools, in registration order.
  pub fn specs(&self) -> Vec<rupi_core::ToolSpec> {
    self
      .tools
      .read()
      .unwrap()
      .iter()
      .filter(|(name, _)| self.is_allowed(name))
      .map(|(_, tool)| tool.spec())
      .collect()
  }

  /// Metadata for permitted tools, for capability reporting and prompts.
  pub fn metadata(&self) -> Vec<ToolMetadata> {
    self
      .tools
      .read()
      .unwrap()
      .iter()
      .filter(|(name, _)| self.is_allowed(name))
      .map(|(_, tool)| tool.metadata.clone())
      .collect()
  }

  pub fn metadata_for(&self, name: &str) -> Option<ToolMetadata> {
    self
      .tools
      .read()
      .unwrap()
      .get(name)
      .map(|tool| tool.metadata.clone())
  }

  /// Whether any permitted tool can change state.
  pub fn has_mutating_tools(&self) -> bool {
    self
      .tools
      .read()
      .unwrap()
      .iter()
      .any(|(name, tool)| self.is_allowed(name) && !tool.metadata.read_only)
  }

  /// Reconcile an uncertain or interrupted tool call against environment state.
  pub fn reconcile(
    &self,
    request: &ToolRequest,
  ) -> Result<rupi_core::ReconciliationStatus, ToolError> {
    self.reconcile_with_risk(request, None)
  }

  /// Reconcile against the exact risk classification captured when the request
  /// crossed the durable `ToolRequested` boundary. A replacement tool with the
  /// same name is not allowed to reinterpret an interrupted mutating call as a
  /// read-only one (or the reverse).
  pub fn reconcile_with_risk(
    &self,
    request: &ToolRequest,
    expected_read_only: Option<bool>,
  ) -> Result<rupi_core::ReconciliationStatus, ToolError> {
    if let Some(error) = &self.configuration_error {
      return Err(ToolError::new(format!(
        "tool registry configuration is invalid: {error}"
      )));
    }
    let tool = {
      let tools = self.tools.read().unwrap();
      let Some(tool) = tools.get(&request.name) else {
        return Err(ToolError::new(format!("unknown tool '{}'", request.name)));
      };
      if expected_read_only.is_some_and(|expected| tool.metadata.read_only != expected) {
        return Err(ToolError::new(format!(
          "tool '{}' risk metadata changed since the interrupted request",
          request.name
        )));
      }
      Arc::clone(&tool.tool)
    };
    tool.reconcile(request)
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
    let mut started = || Ok(());
    self
      .execute_observed(request, progress, cancel, &mut started)
      .expect("the default execution-start observer is infallible")
  }

  /// Execute one call and report the exact execution-start boundary.
  ///
  /// The observer runs after policy, argument, preflight, and approval checks
  /// but before tool code. If durable recording fails, the tool is not invoked.
  pub fn execute_observed(
    &self,
    request: &ToolRequest,
    progress: &mut dyn ToolProgress,
    cancel: &CancelToken,
    on_started: &mut dyn FnMut() -> Result<(), rupi_core::SinkError>,
  ) -> Result<Executed, rupi_core::SinkError> {
    match self.default_gate {
      DefaultGate::Allow => {
        let mut approve = AutoApprove;
        self.dispatch(request, progress, cancel, &mut approve, on_started)
      }
      DefaultGate::Deny => {
        let mut refuse = DenyMutating;
        self.dispatch(request, progress, cancel, &mut refuse, on_started)
      }
    }
  }

  /// Execute with an explicit approval gate and the durable start observer.
  ///
  /// The runtime uses this when its surface can ask a human. Headless callers
  /// should use [`Self::execute_observed`], whose configured default remains
  /// closed unless the operator opted into automatic approval.
  pub fn execute_observed_with_gate(
    &self,
    request: &ToolRequest,
    progress: &mut dyn ToolProgress,
    cancel: &CancelToken,
    gate: &mut dyn ApprovalGate,
    on_started: &mut dyn FnMut() -> Result<(), rupi_core::SinkError>,
  ) -> Result<Executed, rupi_core::SinkError> {
    self.dispatch(request, progress, cancel, gate, on_started)
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
    let mut started = || Ok(());
    self
      .dispatch(request, progress, cancel, gate, &mut started)
      .expect("the interactive execution-start observer is infallible")
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
    on_started: &mut dyn FnMut() -> Result<(), rupi_core::SinkError>,
  ) -> Result<Executed, rupi_core::SinkError> {
    if let Some(error) = &self.configuration_error {
      return Ok(Executed::refused(
        request.clone(),
        format!("tool registry configuration is invalid: {error}"),
      ));
    }
    if cancel.is_cancelled() {
      return Ok(Executed {
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
      });
    }
    let (tool, metadata, arguments_schema) = {
      let tools = self.tools.read().unwrap();
      let Some(tool) = tools.get(&request.name) else {
        return Ok(Executed::refused(
          request.clone(),
          unknown_tool(&request.name, &self.allowed_names()),
        ));
      };
      (
        Arc::clone(&tool.tool),
        tool.metadata.clone(),
        Arc::clone(&tool.schema),
      )
    };
    if !self.is_allowed(&metadata.name) {
      return Ok(Executed::refused(
        request.clone(),
        format!("tool '{}' is denied by policy", metadata.name),
      ));
    }
    if let Err(message) = validate_arguments(&metadata, &request.arguments, &arguments_schema) {
      return Ok(Executed::refused(request.clone(), message));
    }
    if let Err(error) = tool.preflight(request) {
      return Ok(Executed::refused(request.clone(), error.message));
    }
    if !metadata.read_only {
      // `Ask` refuses here as well: nothing in this type can deliver a prompt, so
      // an unanswered question must not read as permission. A surface that can ask
      // returns `Allow`/`Deny` after it has actually asked.
      if let Approval::Deny(reason) | Approval::Ask(reason) =
        gate.decide(&metadata, &request.arguments)
      {
        return Ok(Executed::refused(request.clone(), reason));
      }
    }

    on_started()?;
    let mut sink = Sink {
      inner: progress,
      forwarded: false,
    };
    let context = ToolExecutionContext::new(
      cancel.clone(),
      std::time::Duration::from_millis(self.runtime.shell_timeout_ms),
    );
    let result = tool.execute_with_context(request, &mut sink, &context);

    match result {
      Ok(mut outcome) => {
        let mut full_output = None;
        if let Some(reduction) = reduce::reduce(&outcome.text, self.runtime.max_output_bytes) {
          outcome.text = reduction.text;
          outcome.reduced = true;
          full_output = Some(reduction.full.into_bytes());
        }
        let interrupted = context.is_cancelled_or_expired();
        let state = coerce_state(outcome.state, &metadata, interrupted);
        if state != outcome.state {
          outcome.state = state;
          outcome.is_error = true;
          outcome
            .text
            .push_str(" [completion was not observed before cancellation or deadline]");
        }
        Ok(Executed {
          request: request.clone(),
          outcome,
          state,
          started: true,
          refusal: None,
          full_output,
          cancelled: cancel.is_cancelled(),
        })
      }
      Err(error) => {
        // A harness-level error still has to land in a lifecycle state, because
        // the next model must be able to tell whether the world changed.
        let mut state = error.implied_state(&metadata);
        if context.is_cancelled_or_expired() && state == ToolExecutionState::Started {
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
        Ok(Executed {
          request: request.clone(),
          outcome,
          state,
          started: error.started,
          refusal: Some(error.message),
          full_output: None,
          cancelled: cancel.is_cancelled(),
        })
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

/// Validate the JSON Schema subset used at the tool execution boundary.
///
/// This deliberately avoids compiling a general-purpose validator on the
/// invocation path. Required fields, supplied property types, enums, nested
/// objects/arrays, and `additionalProperties` are enforced recursively; other
/// JSON Schema keywords remain provider guidance, not a runtime guarantee.
fn validate_arguments(
  metadata: &ToolMetadata,
  arguments: &Value,
  schema: &Value,
) -> Result<(), String> {
  if !arguments.is_object() {
    return Err(format!(
      "'{}' arguments must be a JSON object",
      metadata.name
    ));
  }
  validate_schema_value(&metadata.name, "arguments", arguments, schema)
}

fn validate_schema_value(
  tool_name: &str,
  path: &str,
  value: &Value,
  schema: &Value,
) -> Result<(), String> {
  if let Some(kind) = schema.get("type").and_then(Value::as_str) {
    let matches = match kind {
      "null" => value.is_null(),
      "boolean" => value.is_boolean(),
      "object" => value.is_object(),
      "array" => value.is_array(),
      "number" => value.is_number(),
      "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
      "string" => value.is_string(),
      _ => true,
    };
    if !matches {
      return Err(format!("'{tool_name}' argument '{path}' must be {kind}"));
    }
  }

  if let Some(choices) = schema.get("enum").and_then(Value::as_array)
    && !choices.contains(value)
  {
    return Err(format!(
      "'{tool_name}' argument '{path}' is not an allowed value"
    ));
  }

  if let Some(object) = value.as_object() {
    let properties = schema.get("properties").and_then(Value::as_object);
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
      for key in required.iter().filter_map(Value::as_str) {
        if !object.contains_key(key) {
          let missing = if path == "arguments" {
            key.to_string()
          } else {
            format!("{path}.{key}")
          };
          return Err(format!(
            "'{tool_name}' is missing required argument '{missing}'"
          ));
        }
      }
    }

    for (key, child) in object {
      let child_path = format!("{path}.{key}");
      if let Some(child_schema) = properties.and_then(|properties| properties.get(key)) {
        validate_schema_value(tool_name, &child_path, child, child_schema)?;
      } else {
        match schema.get("additionalProperties") {
          Some(Value::Bool(false)) => {
            return Err(format!(
              "'{tool_name}' does not accept argument '{child_path}'"
            ));
          }
          Some(additional_schema @ Value::Object(_)) => {
            validate_schema_value(tool_name, &child_path, child, additional_schema)?;
          }
          _ => {}
        }
      }
    }
  }

  if let (Some(items), Some(array)) = (schema.get("items"), value.as_array()) {
    for (index, item) in array.iter().enumerate() {
      validate_schema_value(tool_name, &format!("{path}[{index}]"), item, items)?;
    }
  }
  Ok(())
}

/// A deadline from the configured shell timeout, for tools that budget their own
/// work without going through the registry.
pub fn deadline_for(policy_timeout_ms: u64) -> Deadline {
  Deadline::new(std::time::Duration::from_millis(policy_timeout_ms))
}
