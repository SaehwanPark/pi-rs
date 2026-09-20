//! Runtime configuration contract.
//!
//! Configuration is deliberately small and JSON-encoded:
//!
//! - **Startup latency.** Parsing one small JSON file is cheap and needs no
//!   parser dependency on the startup path. A TOML surface is a compatibility
//!   question, recorded in the roadmap, not a reason to add a parser now.
//! - **Trust separation.** Project-local configuration is a trust-boundary
//!   concern, so config loading takes an explicit `trusted` flag rather than
//!   deciding silently inside this module.
//! - **Secret hygiene.** Endpoints name an environment variable for the API
//!   key by default. A literal key is accepted for local servers only, and
//!   config serialization never emits one.

use std::{
  collections::{BTreeMap, HashSet},
  fmt,
};

use serde::{Deserialize, Serialize, Serializer, ser::SerializeMap};

use crate::{
  capability::{ModelCapabilities, ModelRef, ReasoningExposure},
  context::ContextProfile,
  provider::ThinkingLevel,
  redact::RedactionPolicy,
  trace::TraceRetention,
};

/// Configuration schema version.
pub const CONFIG_SCHEMA_VERSION: u32 = 1;

/// Default number of model round-trips allowed for one user turn.
pub const DEFAULT_MAX_MODEL_REQUESTS_PER_TURN: u32 = 32;

/// Hard upper bound for the configurable per-turn request budget.
///
/// The budget is intentionally configurable for long-running local-model work,
/// but an unbounded value would turn a provider/tool loop into an accidental
/// runaway process. This is a policy ceiling, not the normal default.
pub const MAX_CONFIGURED_MODEL_REQUESTS_PER_TURN: u32 = 256;

/// Default base URL for remote OpenAI-compatible cloud endpoints.
pub const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

/// One configured model endpoint.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelEndpoint {
  /// Provider id used in `provider/model` references.
  pub provider: String,
  pub model: String,
  /// HTTP base URL for an OpenAI-compatible endpoint, if remote.
  #[serde(
    default,
    skip_serializing_if = "Option::is_none",
    serialize_with = "serialize_optional_redacted_url"
  )]
  pub base_url: Option<String>,
  /// Environment variable holding the credential. Preferred over `api_key`.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub api_key_env: Option<String>,
  /// Literal credential for a local endpoint. Never serialized or debug-printed.
  #[serde(default, skip_serializing)]
  pub api_key: Option<String>,
  pub capabilities: ModelCapabilities,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub max_output_tokens: Option<u64>,
  /// Optional logical connection deadline for this HTTP endpoint.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub connect_timeout_ms: Option<u64>,
  /// Optional logical idle deadline for this HTTP endpoint.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub read_timeout_ms: Option<u64>,
  /// Optional total deadline for one provider request, including model
  /// generation. `None` preserves long-running-session behavior.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub request_timeout_ms: Option<u64>,
}

impl fmt::Debug for ModelEndpoint {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("ModelEndpoint")
      .field("provider", &self.provider)
      .field("model", &self.model)
      .field("base_url", &self.base_url.as_deref().map(redact_url))
      .field("api_key_env", &self.api_key_env)
      .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
      .field("capabilities", &self.capabilities)
      .field("max_output_tokens", &self.max_output_tokens)
      .field("connect_timeout_ms", &self.connect_timeout_ms)
      .field("read_timeout_ms", &self.read_timeout_ms)
      .field("request_timeout_ms", &self.request_timeout_ms)
      .finish()
  }
}

impl ModelEndpoint {
  /// Local endpoint with no credential.
  pub fn local(
    provider: impl Into<String>,
    model: impl Into<String>,
    base_url: impl Into<String>,
    context_window: u64,
  ) -> Self {
    Self {
      provider: provider.into(),
      model: model.into(),
      base_url: Some(base_url.into()),
      api_key_env: None,
      api_key: None,
      capabilities: ModelCapabilities {
        tools: true,
        exposed_reasoning: ReasoningExposure::Native,
        ..ModelCapabilities::text_only(context_window)
      },
      max_output_tokens: None,
      connect_timeout_ms: None,
      read_timeout_ms: None,
      request_timeout_ms: None,
    }
  }

  /// Remote cloud endpoint with an environment variable holding credentials.
  pub fn remote(
    provider: impl Into<String>,
    model: impl Into<String>,
    base_url: Option<impl Into<String>>,
    api_key_env: impl Into<String>,
    context_window: u64,
  ) -> Self {
    Self {
      provider: provider.into(),
      model: model.into(),
      base_url: Some(
        base_url
          .map(Into::into)
          .unwrap_or_else(|| DEFAULT_OPENAI_BASE_URL.into()),
      ),
      api_key_env: Some(api_key_env.into()),
      api_key: None,
      capabilities: ModelCapabilities {
        tools: true,
        exposed_reasoning: ReasoningExposure::Native,
        ..ModelCapabilities::text_only(context_window)
      },
      max_output_tokens: None,
      connect_timeout_ms: None,
      read_timeout_ms: None,
      request_timeout_ms: None,
    }
  }

  pub fn reference(&self) -> ModelRef {
    ModelRef::new(self.provider.clone(), self.model.clone())
  }
}

/// Numeric overrides for experts who need them. Absent means profile-derived.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ContextOverrides {
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub warn_tokens: Option<u64>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub reduce_tokens: Option<u64>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub compact_tokens: Option<u64>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub checkpoint_tokens: Option<u64>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub recent_target_tokens: Option<u64>,
}

/// Tool execution policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolPolicy {
  /// Tools the model may call. Empty means the registered default set.
  #[serde(default)]
  pub allow: Vec<String>,
  /// Tools that are never offered, overriding `allow`.
  #[serde(default)]
  pub deny: Vec<String>,
  /// Run mutating tools without asking. Denied by default: the safe posture is
  /// to ask, and the user may opt out explicitly.
  #[serde(default)]
  pub auto_approve_mutating: bool,
  pub shell_timeout_ms: u64,
  /// Tool output at or above this size is reduced before entering context.
  pub max_output_bytes: u64,
  /// Working directory root for relative paths used by file tools.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub cwd: Option<String>,
  /// Permit the read tool to access an explicitly requested path outside the
  /// workspace. Disabled by default because model-visible reads are an egress
  /// boundary, not merely harmless inspection.
  #[serde(default)]
  pub allow_read_outside: bool,
  /// Permit a grep/exec working-directory walk outside the workspace.
  #[serde(default)]
  pub allow_search_outside: bool,
  /// Permit mutating tools to write outside the workspace. Disabled by default.
  #[serde(default)]
  pub allow_write_outside: bool,
}

impl Default for ToolPolicy {
  fn default() -> Self {
    Self {
      allow: Vec::new(),
      deny: Vec::new(),
      auto_approve_mutating: false,
      shell_timeout_ms: 120_000,
      max_output_bytes: 8 * 1024,
      cwd: None,
      allow_read_outside: false,
      allow_search_outside: false,
      allow_write_outside: false,
    }
  }
}

impl ToolPolicy {
  /// Whether a tool is offered to the model. Deny always wins.
  pub fn is_allowed(&self, name: &str) -> bool {
    if self.deny.iter().any(|deny| deny == name) {
      return false;
    }
    self.allow.is_empty() || self.allow.iter().any(|allow| allow == name)
  }
}

/// UI preferences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiConfig {
  /// Show reasoning-like output at all.
  pub show_reasoning: bool,
  /// Collapse reasoning bodies by default.
  pub collapse_reasoning: bool,
  /// Truncate long tool output in the transcript.
  pub collapse_tool_output: bool,
}

impl Default for UiConfig {
  fn default() -> Self {
    Self {
      show_reasoning: true,
      collapse_reasoning: true,
      collapse_tool_output: true,
    }
  }
}

/// Safety limits that apply to one runtime turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeLimits {
  /// Maximum model round-trips for one user input, including retries and failover.
  #[serde(default = "default_max_model_requests_per_turn")]
  pub max_model_requests_per_turn: u32,
  /// Optional number of model requests that may invoke tools without making
  /// configured progress before the next request is narrowed to progress tools.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub max_model_requests_without_progress: Option<u32>,
  /// Tool names that count as progress when the progress boundary is active.
  /// An empty list uses every permitted mutating tool instead.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub progress_tool_names: Vec<String>,
}

impl Default for RuntimeLimits {
  fn default() -> Self {
    Self {
      max_model_requests_per_turn: DEFAULT_MAX_MODEL_REQUESTS_PER_TURN,
      max_model_requests_without_progress: None,
      progress_tool_names: Vec::new(),
    }
  }
}

fn default_max_model_requests_per_turn() -> u32 {
  DEFAULT_MAX_MODEL_REQUESTS_PER_TURN
}

/// Runtime configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeConfig {
  pub version: u32,
  pub primary: ModelRef,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub backup: Option<ModelRef>,
  #[serde(default)]
  pub context_profile: ContextProfile,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub context_overrides: Option<ContextOverrides>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub adaptive_context: Option<bool>,
  pub thinking: ThinkingLevel,
  /// State root for sessions, traces, and checkpoints.
  pub state_dir: String,
  #[serde(default)]
  pub endpoints: Vec<ModelEndpoint>,
  #[serde(default)]
  pub tools: ToolPolicy,
  #[serde(default)]
  pub ui: UiConfig,
  #[serde(default)]
  pub limits: RuntimeLimits,
  #[serde(default)]
  pub trace: TraceRetention,
  #[serde(default)]
  pub redaction: RedactionPolicy,
  #[serde(default)]
  pub mcp_servers: Vec<McpServerConfig>,
}

/// Configuration for an external MCP server.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerConfig {
  pub name: String,
  /// Stdio command. Required when `url` is absent.
  pub command: String,
  #[serde(default)]
  pub args: Vec<String>,
  #[serde(default, serialize_with = "serialize_redacted_values")]
  pub env: BTreeMap<String, String>,
  #[serde(default)]
  pub enabled: bool,
  #[serde(default)]
  pub read_only_tools: Vec<String>,
  /// Streamable HTTP endpoint. When present, no child process is spawned.
  #[serde(
    default,
    skip_serializing_if = "Option::is_none",
    serialize_with = "serialize_optional_redacted_url"
  )]
  pub url: Option<String>,
  /// Additional HTTP headers for a network MCP endpoint.
  #[serde(
    default,
    skip_serializing_if = "BTreeMap::is_empty",
    serialize_with = "serialize_redacted_values"
  )]
  pub headers: BTreeMap<String, String>,
}

impl fmt::Debug for McpServerConfig {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    let header_names: Vec<&str> = self.headers.keys().map(String::as_str).collect();
    formatter
      .debug_struct("McpServerConfig")
      .field("name", &self.name)
      .field("command", &self.command)
      .field("args", &self.args)
      .field("env", &self.env.keys().collect::<Vec<_>>())
      .field("enabled", &self.enabled)
      .field("read_only_tools", &self.read_only_tools)
      .field("url", &self.url.as_deref().map(redact_url))
      .field("header_names", &header_names)
      .finish()
  }
}

impl McpServerConfig {
  pub fn new(name: impl Into<String>, command: impl Into<String>) -> Self {
    Self {
      name: name.into(),
      command: command.into(),
      args: Vec::new(),
      env: BTreeMap::new(),
      enabled: false,
      read_only_tools: Vec::new(),
      url: None,
      headers: BTreeMap::new(),
    }
  }

  pub fn with_args(mut self, args: Vec<String>) -> Self {
    self.args = args;
    self
  }

  pub fn with_env(mut self, env: BTreeMap<String, String>) -> Self {
    self.env = env;
    self
  }

  pub fn with_enabled(mut self, enabled: bool) -> Self {
    self.enabled = enabled;
    self
  }

  pub fn with_url(mut self, url: impl Into<String>) -> Self {
    self.url = Some(url.into());
    self
  }

  pub fn with_headers(mut self, headers: BTreeMap<String, String>) -> Self {
    self.headers = headers;
    self
  }

  pub fn is_network(&self) -> bool {
    self.url.is_some()
  }
}

impl RuntimeConfig {
  /// Minimal valid configuration for one primary model.
  pub fn new(primary: ModelRef, state_dir: impl Into<String>) -> Self {
    Self {
      version: CONFIG_SCHEMA_VERSION,
      primary,
      backup: None,
      context_profile: ContextProfile::default(),
      context_overrides: None,
      adaptive_context: None,
      thinking: ThinkingLevel::default(),
      state_dir: state_dir.into(),
      endpoints: Vec::new(),
      tools: ToolPolicy::default(),
      ui: UiConfig::default(),
      limits: RuntimeLimits::default(),
      trace: TraceRetention::default(),
      redaction: RedactionPolicy::default(),
      mcp_servers: Vec::new(),
    }
  }

  pub fn parse(json: &str) -> Result<Self, ConfigError> {
    let config: Self =
      serde_json::from_str(json).map_err(|error| ConfigError(error.to_string()))?;
    config.validate()?;
    Ok(config)
  }

  pub fn validate(&self) -> Result<(), ConfigError> {
    if self.version > CONFIG_SCHEMA_VERSION {
      return Err(ConfigError(format!(
        "config schema version {} is newer than this build supports ({CONFIG_SCHEMA_VERSION})",
        self.version
      )));
    }
    if self.state_dir.trim().is_empty() {
      return Err(ConfigError("state_dir is required".into()));
    }
    if self.limits.max_model_requests_per_turn == 0 {
      return Err(ConfigError(
        "limits.max_model_requests_per_turn must be greater than zero".into(),
      ));
    }
    if self.limits.max_model_requests_per_turn > MAX_CONFIGURED_MODEL_REQUESTS_PER_TURN {
      return Err(ConfigError(format!(
        "limits.max_model_requests_per_turn must not exceed {MAX_CONFIGURED_MODEL_REQUESTS_PER_TURN}"
      )));
    }
    if let Some(limit) = self.limits.max_model_requests_without_progress {
      if limit == 0 {
        return Err(ConfigError(
          "limits.max_model_requests_without_progress must be greater than zero".into(),
        ));
      }
      if limit > MAX_CONFIGURED_MODEL_REQUESTS_PER_TURN {
        return Err(ConfigError(format!(
          "limits.max_model_requests_without_progress must not exceed {MAX_CONFIGURED_MODEL_REQUESTS_PER_TURN}"
        )));
      }
    } else if !self.limits.progress_tool_names.is_empty() {
      return Err(ConfigError(
        "limits.progress_tool_names requires max_model_requests_without_progress".into(),
      ));
    }
    let mut progress_tool_names = HashSet::new();
    for name in &self.limits.progress_tool_names {
      if name.trim().is_empty() {
        return Err(ConfigError(
          "limits.progress_tool_names must not contain empty names".into(),
        ));
      }
      if !progress_tool_names.insert(name) {
        return Err(ConfigError(format!(
          "limits.progress_tool_names contains duplicate tool '{name}'"
        )));
      }
    }
    if let Some(backup) = &self.backup {
      if backup == &self.primary {
        return Err(ConfigError(
          "backup model must differ from primary; a self-backup hides failures".into(),
        ));
      }
    }
    let mut mcp_names = HashSet::new();
    for mcp in &self.mcp_servers {
      if mcp.name.trim().is_empty() {
        return Err(ConfigError("MCP server name must not be empty".into()));
      }
      if !mcp_names.insert(mcp.name.as_str()) {
        return Err(ConfigError(format!(
          "MCP server name '{}' is duplicated",
          mcp.name
        )));
      }
      if let Some(url) = &mcp.url {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
          return Err(ConfigError(format!(
            "MCP server {} URL must start with http:// or https://",
            mcp.name
          )));
        }
        if url_has_userinfo(url) {
          return Err(ConfigError(format!(
            "MCP server {} URL must not contain userinfo credentials",
            mcp.name
          )));
        }
      } else if mcp.command.trim().is_empty() {
        return Err(ConfigError(format!(
          "MCP server {} needs a command or URL",
          mcp.name
        )));
      }
      for (name, value) in &mcp.headers {
        if !valid_http_header_name(name)
          || value.contains('\r')
          || value.contains('\n')
          || is_protocol_header(name)
        {
          return Err(ConfigError(format!(
            "MCP server {} has an invalid HTTP header",
            mcp.name
          )));
        }
      }
    }
    for endpoint in &self.endpoints {
      if endpoint.provider.trim().is_empty() || endpoint.model.trim().is_empty() {
        return Err(ConfigError(
          "endpoint provider and model are required".into(),
        ));
      }
      if endpoint.capabilities.context_window == 0 {
        return Err(ConfigError(format!(
          "endpoint {}/{} must declare a context window",
          endpoint.provider, endpoint.model
        )));
      }
      if endpoint.base_url.as_deref().is_some_and(url_has_userinfo) {
        return Err(ConfigError(format!(
          "endpoint {}/{} URL must not contain userinfo credentials",
          endpoint.provider, endpoint.model
        )));
      }
      for (name, timeout) in [
        ("connect_timeout_ms", endpoint.connect_timeout_ms),
        ("read_timeout_ms", endpoint.read_timeout_ms),
        ("request_timeout_ms", endpoint.request_timeout_ms),
      ] {
        if timeout == Some(0) {
          return Err(ConfigError(format!(
            "endpoint {}/{} {name} must be greater than zero",
            endpoint.provider, endpoint.model
          )));
        }
      }
    }
    // Endpoints are optional: the CLI can configure a model directly. When the
    // user does declare endpoints, every model the runtime may use has to be
    // reachable through one of them, or a failover would fail at the worst
    // possible moment.
    if !self.endpoints.is_empty() {
      let mut required: Vec<(&str, &ModelRef)> = vec![("primary", &self.primary)];
      if let Some(backup) = &self.backup {
        required.push(("backup", backup));
      }
      for (role, model) in required {
        if !self
          .endpoints
          .iter()
          .any(|endpoint| endpoint.provider == model.provider && endpoint.model == model.model)
        {
          return Err(ConfigError(format!(
            "{role} model {model} has no endpoint entry"
          )));
        }
      }
    }
    Ok(())
  }

  pub fn endpoint_for(&self, model: &ModelRef) -> Option<&ModelEndpoint> {
    self
      .endpoints
      .iter()
      .find(|endpoint| endpoint.provider == model.provider && endpoint.model == model.model)
  }

  /// Serialize, with literal credentials stripped.
  ///
  /// Config files are routinely pasted into issues and committed by accident,
  /// so writing a secret back out must be impossible rather than discouraged.
  pub fn to_json_string(&self) -> Result<String, ConfigError> {
    let value = serde_json::to_value(self).map_err(|error| ConfigError(error.to_string()))?;
    let mut value = value;
    if let Some(endpoints) = value.get_mut("endpoints").and_then(|v| v.as_array_mut()) {
      for endpoint in endpoints {
        if let Some(object) = endpoint.as_object_mut() {
          object.remove("api_key");
        }
      }
    }
    if let Some(servers) = value.get_mut("mcp_servers").and_then(|v| v.as_array_mut()) {
      for server in servers {
        if let Some(url) = server
          .get("url")
          .and_then(|value| value.as_str())
          .map(str::to_owned)
        {
          server["url"] = serde_json::Value::String(redact_url(&url));
        }
        if let Some(headers) = server.get_mut("headers").and_then(|v| v.as_object_mut()) {
          for value in headers.values_mut() {
            *value = serde_json::Value::String("[redacted]".into());
          }
        }
      }
    }
    serde_json::to_string_pretty(&value).map_err(|error| ConfigError(error.to_string()))
  }
}

fn serialize_optional_redacted_url<S>(
  url: &Option<String>,
  serializer: S,
) -> Result<S::Ok, S::Error>
where
  S: Serializer,
{
  url.as_deref().map(redact_url).serialize(serializer)
}

fn serialize_redacted_values<S>(
  values: &BTreeMap<String, String>,
  serializer: S,
) -> Result<S::Ok, S::Error>
where
  S: Serializer,
{
  let mut map = serializer.serialize_map(Some(values.len()))?;
  for name in values.keys() {
    map.serialize_entry(name, "[redacted]")?;
  }
  map.end()
}

fn valid_http_header_name(name: &str) -> bool {
  !name.is_empty()
    && name.bytes().all(|byte| {
      byte.is_ascii_alphanumeric()
        || matches!(
          byte,
          b'!'
            | b'#'
            | b'$'
            | b'%'
            | b'&'
            | b'\''
            | b'*'
            | b'+'
            | b'-'
            | b'.'
            | b'^'
            | b'_'
            | b'`'
            | b'|'
            | b'~'
        )
    })
}

fn is_protocol_header(name: &str) -> bool {
  matches!(
    name.to_ascii_lowercase().as_str(),
    "accept"
      | "content-type"
      | "content-length"
      | "host"
      | "mcp-session-id"
      | "mcp-protocol-version"
  )
}

fn redact_url(url: &str) -> String {
  let safe = if let Some((scheme, authority_and_path)) = url.split_once("://") {
    let authority_end = authority_and_path
      .find(['/', '?', '#'])
      .unwrap_or(authority_and_path.len());
    let authority = &authority_and_path[..authority_end];
    if let Some(at) = authority.rfind('@') {
      format!(
        "{scheme}://[redacted]@{}{}",
        &authority[at + 1..],
        &authority_and_path[authority_end..]
      )
    } else {
      url.to_string()
    }
  } else {
    url.to_string()
  };
  if let Some((base, _)) = safe.split_once('?') {
    return format!("{base}?[redacted]");
  }
  if let Some((base, _)) = safe.split_once('#') {
    return format!("{base}#[redacted]");
  }
  safe
}

fn url_has_userinfo(url: &str) -> bool {
  let Some((_, authority_and_path)) = url.split_once("://") else {
    return false;
  };
  authority_and_path
    .split_once('/')
    .map(|(authority, _)| authority.contains('@'))
    .unwrap_or_else(|| authority_and_path.contains('@'))
}

/// Configuration problem, in operator terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(&self.0)
  }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
  use super::*;

  fn sample_config() -> RuntimeConfig {
    let mut config = RuntimeConfig::new(
      ModelRef::parse("local/qwen").unwrap(),
      "/home/user/.local/state/rupi",
    );
    config.endpoints.push(ModelEndpoint::local(
      "local",
      "qwen",
      "http://127.0.0.1:8080/v1",
      131_072,
    ));
    config
  }

  #[test]
  fn round_trips_through_json() {
    let config = sample_config();
    let json = config.to_json_string().unwrap();
    let parsed = RuntimeConfig::parse(&json).unwrap();
    assert_eq!(parsed, config);
    assert_eq!(parsed.version, CONFIG_SCHEMA_VERSION);
    assert_eq!(parsed.context_profile, ContextProfile::Balanced);
    assert!(!parsed.tools.allow_read_outside);
    assert!(!parsed.tools.allow_search_outside);
    assert!(!parsed.tools.allow_write_outside);
    assert_eq!(
      parsed.limits.max_model_requests_per_turn,
      DEFAULT_MAX_MODEL_REQUESTS_PER_TURN
    );
    assert_eq!(parsed.limits.max_model_requests_without_progress, None);
    assert!(parsed.limits.progress_tool_names.is_empty());
  }

  #[test]
  fn request_budget_is_configurable_with_a_hard_ceiling() {
    let mut config = sample_config();
    config.limits.max_model_requests_per_turn = 64;
    let json = config.to_json_string().unwrap();
    assert_eq!(
      RuntimeConfig::parse(&json)
        .unwrap()
        .limits
        .max_model_requests_per_turn,
      64
    );

    config.limits.max_model_requests_per_turn = 0;
    assert!(
      config
        .validate()
        .unwrap_err()
        .0
        .contains("greater than zero")
    );
    config.limits.max_model_requests_per_turn = MAX_CONFIGURED_MODEL_REQUESTS_PER_TURN + 1;
    assert!(config.validate().unwrap_err().0.contains("must not exceed"));
  }

  #[test]
  fn progress_boundary_round_trips_and_rejects_ambiguous_limits() {
    let mut config = sample_config();
    config.limits.max_model_requests_without_progress = Some(2);
    config.limits.progress_tool_names = vec!["write".into(), "edit".into()];
    let parsed = RuntimeConfig::parse(&config.to_json_string().unwrap()).unwrap();
    assert_eq!(parsed.limits.max_model_requests_without_progress, Some(2));
    assert_eq!(parsed.limits.progress_tool_names, ["write", "edit"]);

    config.limits.max_model_requests_without_progress = Some(0);
    assert!(
      config
        .validate()
        .unwrap_err()
        .0
        .contains("max_model_requests_without_progress")
    );

    config.limits.max_model_requests_without_progress = None;
    assert!(
      config
        .validate()
        .unwrap_err()
        .0
        .contains("requires max_model_requests_without_progress")
    );

    config.limits.max_model_requests_without_progress = Some(2);
    config.limits.progress_tool_names = vec!["write".into(), "write".into()];
    assert!(config.validate().unwrap_err().0.contains("duplicate tool"));
  }

  #[test]
  fn older_config_without_limits_uses_the_safe_default() {
    let mut value = serde_json::to_value(sample_config()).unwrap();
    value.as_object_mut().unwrap().remove("limits");
    let parsed = RuntimeConfig::parse(&serde_json::to_string(&value).unwrap()).unwrap();
    assert_eq!(
      parsed.limits.max_model_requests_per_turn,
      DEFAULT_MAX_MODEL_REQUESTS_PER_TURN
    );
  }

  #[test]
  fn filesystem_policy_flags_round_trip_explicit_widening() {
    let mut config = sample_config();
    config.tools.allow_read_outside = true;
    config.tools.allow_search_outside = true;
    config.tools.allow_write_outside = true;
    let parsed = RuntimeConfig::parse(&config.to_json_string().unwrap()).unwrap();
    assert!(parsed.tools.allow_read_outside);
    assert!(parsed.tools.allow_search_outside);
    assert!(parsed.tools.allow_write_outside);
  }

  #[test]
  fn endpoint_timeout_overrides_round_trip_and_reject_zero() {
    let mut config = sample_config();
    config.endpoints[0].connect_timeout_ms = Some(750);
    config.endpoints[0].read_timeout_ms = Some(2_500);
    config.endpoints[0].request_timeout_ms = Some(120_000);
    let parsed = RuntimeConfig::parse(&config.to_json_string().unwrap()).unwrap();
    assert_eq!(parsed.endpoints[0].connect_timeout_ms, Some(750));
    assert_eq!(parsed.endpoints[0].read_timeout_ms, Some(2_500));
    assert_eq!(parsed.endpoints[0].request_timeout_ms, Some(120_000));

    config.endpoints[0].read_timeout_ms = Some(0);
    assert!(config.validate().unwrap_err().0.contains("read_timeout_ms"));

    config.endpoints[0].read_timeout_ms = None;
    config.endpoints[0].request_timeout_ms = Some(0);
    assert!(
      config
        .validate()
        .unwrap_err()
        .0
        .contains("request_timeout_ms")
    );
  }

  #[test]
  fn literal_credentials_are_never_written_or_debug_printed() {
    let mut config = sample_config();
    config.endpoints[0].api_key = Some("local-debug-key".into());
    config.redaction.literals = vec!["literal-redaction-secret".into()];
    config.mcp_servers.push(
      McpServerConfig::new("remote", "command")
        .with_env(BTreeMap::from([("TOKEN".into(), "env-secret".into())]))
        .with_headers(BTreeMap::from([(
          "authorization".into(),
          "Bearer mcp-secret".into(),
        )])),
    );
    let json = serde_json::to_string(&config.endpoints[0]).unwrap();
    assert!(
      !json.contains("local-debug-key"),
      "endpoint serialization must not carry secrets: {json}"
    );
    let endpoint_debug = format!("{:?}", config.endpoints[0]);
    assert!(
      !endpoint_debug.contains("local-debug-key"),
      "{endpoint_debug}"
    );
    let runtime_debug = format!("{config:?}");
    assert!(
      !runtime_debug.contains("local-debug-key"),
      "{runtime_debug}"
    );
    assert!(
      !runtime_debug.contains("literal-redaction-secret"),
      "{runtime_debug}"
    );
    assert!(!runtime_debug.contains("env-secret"), "{runtime_debug}");
    assert!(
      !runtime_debug.contains("Bearer mcp-secret"),
      "{runtime_debug}"
    );
    let direct_json = serde_json::to_string(&config).unwrap();
    assert!(
      !direct_json.contains("literal-redaction-secret"),
      "{direct_json}"
    );
    assert!(!direct_json.contains("env-secret"), "{direct_json}");
    assert!(!direct_json.contains("Bearer mcp-secret"), "{direct_json}");
    let json = config.to_json_string().unwrap();
    assert!(
      !json.contains("local-debug-key"),
      "config output must not carry secrets: {json}"
    );
    assert!(json.contains("http://127.0.0.1:8080/v1"));
    assert!(!json.contains("literal-redaction-secret"), "{json}");
    assert!(!json.contains("env-secret"), "{json}");
    assert!(!json.contains("Bearer mcp-secret"), "{json}");
  }

  #[test]
  fn endpoint_userinfo_credentials_are_rejected() {
    let mut config = sample_config();
    config.endpoints[0].base_url = Some("https://user:secret@example.test/v1".into());
    assert!(config.validate().unwrap_err().0.contains("userinfo"));
  }

  #[test]
  fn newer_schema_version_is_refused() {
    let mut config = sample_config();
    config.version = CONFIG_SCHEMA_VERSION + 1;
    let error = config.validate().unwrap_err();
    assert!(error.0.contains("newer"), "{error}");
  }

  #[test]
  fn self_backup_is_refused() {
    let mut config = sample_config();
    config.backup = Some(config.primary.clone());
    assert!(config.validate().unwrap_err().0.contains("differ"));
  }

  #[test]
  fn declared_endpoints_must_cover_every_used_model() {
    let mut config = sample_config();
    config.backup = Some(ModelRef::parse("cloud/gpt").unwrap());
    let error = config.validate().unwrap_err();
    assert!(error.0.contains("backup model cloud/gpt"), "{error}");

    let mut other = sample_config();
    other.primary = ModelRef::parse("other/model").unwrap();
    let error = other.validate().unwrap_err();
    assert!(error.0.contains("primary model other/model"), "{error}");
  }

  #[test]
  fn empty_endpoint_list_allows_cli_only_configuration() {
    let mut config = sample_config();
    config.endpoints.clear();
    assert_eq!(config.validate(), Ok(()));
  }

  #[test]
  fn tool_policy_deny_wins() {
    let mut policy = ToolPolicy::default();
    assert!(policy.is_allowed("read"));
    policy.allow = vec!["read".into(), "grep".into()];
    assert!(policy.is_allowed("read"));
    assert!(!policy.is_allowed("exec"));
    policy.allow.clear();
    policy.deny = vec!["exec".into()];
    assert!(policy.is_allowed("read"));
    assert!(!policy.is_allowed("exec"));
    assert!(!policy.auto_approve_mutating);
  }

  #[test]
  fn network_mcp_config_round_trips_and_requires_safe_headers() {
    let mut config = sample_config();
    let server = McpServerConfig::new("remote", "")
      .with_url("https://mcp.example.test/rpc")
      .with_headers(BTreeMap::from([(
        "authorization".into(),
        "Bearer test".into(),
      )]));
    config.mcp_servers.push(server);
    let json = config.to_json_string().unwrap();
    let parsed = RuntimeConfig::parse(&json).unwrap();
    assert_eq!(
      parsed.mcp_servers[0].url.as_deref(),
      Some("https://mcp.example.test/rpc")
    );
    assert_eq!(parsed.mcp_servers[0].headers["authorization"], "[redacted]");
    assert!(
      !json.contains("Bearer test"),
      "MCP header values must be redacted"
    );

    let mut invalid = config;
    invalid.mcp_servers[0]
      .headers
      .insert("x\nname".into(), "x".into());
    assert!(invalid.validate().unwrap_err().0.contains("header"));
  }

  #[test]
  fn mcp_config_redacts_url_queries_and_rejects_duplicates_and_protocol_headers() {
    let mut config = sample_config();
    config.mcp_servers.push(
      McpServerConfig::new("remote", "")
        .with_url("https://mcp.example.test/rpc?token=secret-value"),
    );
    let json = config.to_json_string().unwrap();
    assert!(
      !json.contains("secret-value"),
      "MCP URL query leaked: {json}"
    );
    assert!(json.contains("https://mcp.example.test/rpc?[redacted]"));
    let debug = format!("{:?}", config.mcp_servers[0]);
    assert!(
      !debug.contains("secret-value"),
      "MCP Debug leaked URL query: {debug}"
    );

    let mut duplicate = config.clone();
    duplicate
      .mcp_servers
      .push(McpServerConfig::new("remote", "command"));
    assert!(duplicate.validate().unwrap_err().0.contains("duplicated"));

    let mut reserved = sample_config();
    reserved.mcp_servers.push(
      McpServerConfig::new("reserved", "")
        .with_url("https://mcp.example.test/rpc")
        .with_headers(BTreeMap::from([(
          "MCP-Protocol-Version".into(),
          "spoof".into(),
        )])),
    );
    assert!(reserved.validate().unwrap_err().0.contains("header"));
  }

  #[test]
  fn network_mcp_requires_http_url_or_stdio_command() {
    let mut config = sample_config();
    config
      .mcp_servers
      .push(McpServerConfig::new("remote", "").with_url("file:///tmp/mcp"));
    assert!(config.validate().unwrap_err().0.contains("URL"));

    let mut no_endpoint = sample_config();
    no_endpoint
      .mcp_servers
      .push(McpServerConfig::new("missing", ""));
    assert!(
      no_endpoint
        .validate()
        .unwrap_err()
        .0
        .contains("command or URL")
    );
  }

  #[test]
  fn defaults_are_conservative() {
    let config = sample_config();
    assert!(config.trace.raw_payload == crate::trace::RawPayloadCapture::Disabled);
    assert!(config.redaction.enabled);
    assert!(config.ui.collapse_reasoning);
    assert_eq!(config.thinking, ThinkingLevel::Medium);
    assert_eq!(config.backup, None);
  }
}
