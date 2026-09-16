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

use serde::{Deserialize, Serialize};

use crate::{
  capability::{ModelCapabilities, ModelRef, ReasoningExposure},
  context::ContextProfile,
  provider::ThinkingLevel,
  redact::RedactionPolicy,
  trace::TraceRetention,
};

/// Configuration schema version.
pub const CONFIG_SCHEMA_VERSION: u32 = 1;

/// Default base URL for remote OpenAI-compatible cloud endpoints.
pub const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

/// One configured model endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelEndpoint {
  /// Provider id used in `provider/model` references.
  pub provider: String,
  pub model: String,
  /// HTTP base URL for an OpenAI-compatible endpoint, if remote.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub base_url: Option<String>,
  /// Environment variable holding the credential. Preferred over `api_key`.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub api_key_env: Option<String>,
  /// Literal credential for a local endpoint. Never serialized back out.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub api_key: Option<String>,
  pub capabilities: ModelCapabilities,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub max_output_tokens: Option<u64>,
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
  #[serde(default)]
  pub env: BTreeMap<String, String>,
  #[serde(default)]
  pub enabled: bool,
  #[serde(default)]
  pub read_only_tools: Vec<String>,
  /// Streamable HTTP endpoint. When present, no child process is spawned.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub url: Option<String>,
  /// Additional HTTP headers for a network MCP endpoint.
  #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
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
  if let Some((base, _)) = url.split_once('?') {
    return format!("{base}?[redacted]");
  }
  if let Some((base, _)) = url.split_once('#') {
    return format!("{base}#[redacted]");
  }
  url.to_string()
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
      "/home/user/.local/state/pi-rs",
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
  fn literal_credentials_are_never_written_out() {
    let mut config = sample_config();
    config.endpoints[0].api_key = Some("local-debug-key".into());
    let json = config.to_json_string().unwrap();
    assert!(
      !json.contains("local-debug-key"),
      "config output must not carry secrets: {json}"
    );
    assert!(json.contains("http://127.0.0.1:8080/v1"));
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
