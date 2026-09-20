//! Endpoint configuration for the OpenAI-compatible adapter.
//!
//! The knobs here exist because the "OpenAI-compatible" family is not one
//! implementation. Servers disagree on the token-limit field name, on how the
//! thinking switch is spelled, and on whether SSE is implemented correctly.
//! Each difference is expressed as one explicit, tested switch instead of a
//! guess inside the request path.

use std::{
  collections::BTreeMap,
  fmt,
  hash::{Hash, Hasher},
  sync::{Mutex, OnceLock},
  time::Duration,
};

use rupi_core::{CapabilityGap, ModelCapabilities, ModelEndpoint, ReasoningExposure};
use serde::{Deserialize, Serialize, Serializer, ser::SerializeMap};

/// Base URL used when nothing is configured.
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// Largest error body read from a provider.
pub(crate) const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// Configuration for one OpenAI-compatible endpoint.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderConfig {
  /// Provider id used in `provider/model` references.
  pub id: String,
  /// Human-readable name.
  pub name: String,
  /// API root, for example `http://127.0.0.1:8080/v1`.
  #[serde(serialize_with = "serialize_redacted_url")]
  pub base_url: String,
  /// Model id used when a request does not name one.
  pub model: String,
  /// Literal credential. `api_key_env` is preferred; this field is never
  /// serialized back out.
  #[serde(skip_serializing)]
  pub api_key: Option<String>,
  /// Environment variable holding the credential.
  pub api_key_env: Option<String>,
  /// Extra headers, for example a gateway routing hint. Values are redacted
  /// when this configuration is serialized because arbitrary headers may carry
  /// credentials even when they are not named `api_key`.
  #[serde(serialize_with = "serialize_redacted_values")]
  pub headers: BTreeMap<String, String>,
  /// Declared capabilities. A claim, never a discovery result.
  pub capabilities: ModelCapabilities,
  /// Output ceiling to request when the caller does not set one.
  pub max_output_tokens: Option<u64>,
  /// Which token-limit field this endpoint accepts.
  pub max_tokens_field: MaxTokensField,
  /// How this endpoint wants the thinking switch expressed.
  pub thinking_input: ThinkingInput,
  /// Whether to stream. One-shot mode is the documented workaround for
  /// gateways that corrupt SSE, not the default.
  pub stream: bool,
  pub connect_timeout_ms: u64,
  /// Logical idle budget for waiting on response headers or the next body
  /// event. The adapter's worker boundary keeps cancellation independent from
  /// this potentially long blocking socket timeout.
  pub read_timeout_ms: u64,
  /// Optional total budget for one provider request, including model
  /// generation. `None` keeps long-running sessions unbounded.
  pub request_timeout_ms: Option<u64>,
}

impl fmt::Debug for ProviderConfig {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    let header_names: Vec<&str> = self.headers.keys().map(String::as_str).collect();
    formatter
      .debug_struct("ProviderConfig")
      .field("id", &self.id)
      .field("name", &self.name)
      .field("base_url", &redact_url(&self.base_url))
      .field("model", &self.model)
      .field("api_key", &self.api_key.as_ref().map(|_| "[redacted]"))
      .field("api_key_env", &self.api_key_env)
      .field("header_names", &header_names)
      .field("capabilities", &self.capabilities)
      .field("max_output_tokens", &self.max_output_tokens)
      .field("max_tokens_field", &self.max_tokens_field)
      .field("thinking_input", &self.thinking_input)
      .field("stream", &self.stream)
      .field("connect_timeout_ms", &self.connect_timeout_ms)
      .field("read_timeout_ms", &self.read_timeout_ms)
      .field("request_timeout_ms", &self.request_timeout_ms)
      .finish()
  }
}

impl Default for ProviderConfig {
  fn default() -> Self {
    Self {
      id: "openai".into(),
      name: "OpenAI compatible".into(),
      base_url: DEFAULT_BASE_URL.into(),
      model: String::new(),
      api_key: None,
      api_key_env: Some("OPENAI_API_KEY".into()),
      headers: BTreeMap::new(),
      capabilities: ModelCapabilities {
        text: true,
        images: false,
        tools: false,
        exposed_reasoning: ReasoningExposure::None,
        context_window: 8_192,
        max_output_tokens: None,
      },
      max_output_tokens: None,
      max_tokens_field: MaxTokensField::default(),
      thinking_input: ThinkingInput::default(),
      stream: true,
      connect_timeout_ms: 10_000,
      // The adapter retries bounded socket polls across quiet reasoning
      // intervals, so this remains a generous logical idle budget for callers
      // while cancellation is still observed promptly.
      read_timeout_ms: 300_000,
      request_timeout_ms: None,
    }
  }
}

/// Which token-limit field an endpoint accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensField {
  /// `max_tokens`. Accepted by OpenAI, vLLM, and llama.cpp OpenAI-compatible
  /// surfaces, so it is the default for a harness that expects local servers.
  #[default]
  MaxTokens,
  /// `max_completion_tokens`, the newer OpenAI name.
  MaxCompletionTokens,
}

/// How to ask an endpoint for thinking output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingInput {
  /// Send nothing. For endpoints that reject unknown fields.
  None,
  /// OpenAI `reasoning_effort`.
  #[default]
  ReasoningEffort,
  /// `chat_template_kwargs: { "thinking": bool }`, as llama.cpp builds expect.
  ChatTemplateThinking,
}

/// A configuration that cannot produce a working adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildError {
  /// A required field is missing or malformed.
  Invalid(&'static str),
  /// The declared capabilities cannot serve the harness contract.
  MissingCapability(CapabilityGap),
}

impl fmt::Display for BuildError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Invalid(field) => write!(f, "invalid provider config: {field}"),
      Self::MissingCapability(gap) => write!(f, "provider cannot serve session needs: {gap}"),
    }
  }
}

impl std::error::Error for BuildError {}

impl ProviderConfig {
  /// Config for a local endpoint with no credential.
  pub fn local(
    id: impl Into<String>,
    model: impl Into<String>,
    base_url: impl Into<String>,
    context_window: u64,
  ) -> Self {
    Self {
      id: id.into(),
      model: model.into(),
      base_url: base_url.into(),
      api_key: None,
      api_key_env: None,
      capabilities: ModelCapabilities::text_only(context_window),
      ..Self::default()
    }
  }

  /// Config for a remote cloud endpoint with an environment variable holding the credential.
  pub fn remote(
    id: impl Into<String>,
    model: impl Into<String>,
    base_url: Option<impl Into<String>>,
    api_key_env: impl Into<String>,
    context_window: u64,
  ) -> Self {
    Self {
      id: id.into(),
      model: model.into(),
      base_url: base_url
        .map(Into::into)
        .unwrap_or_else(|| DEFAULT_BASE_URL.into()),
      api_key: None,
      api_key_env: Some(api_key_env.into()),
      capabilities: ModelCapabilities::text_only(context_window),
      ..Self::default()
    }
  }

  /// Derive adapter config from a runtime-config endpoint.
  ///
  /// The credential is resolved from the environment exactly once, here, so
  /// nothing downstream has to know where a secret came from.
  pub fn from_endpoint(endpoint: &ModelEndpoint) -> Result<Self, BuildError> {
    let base_url = endpoint
      .base_url
      .clone()
      .filter(|url| !url.trim().is_empty())
      .ok_or(BuildError::Invalid(
        "base_url is required for an HTTP endpoint",
      ))?;
    let mut config = Self {
      id: endpoint.provider.clone(),
      model: endpoint.model.clone(),
      base_url,
      capabilities: endpoint.capabilities.clone(),
      max_output_tokens: endpoint
        .max_output_tokens
        .or(endpoint.capabilities.max_output_tokens),
      connect_timeout_ms: endpoint
        .connect_timeout_ms
        .unwrap_or(Self::default().connect_timeout_ms),
      read_timeout_ms: endpoint
        .read_timeout_ms
        .unwrap_or(Self::default().read_timeout_ms),
      request_timeout_ms: endpoint.request_timeout_ms,
      api_key: endpoint.api_key.clone(),
      api_key_env: endpoint.api_key_env.clone(),
      ..Self::default()
    };
    if config.api_key.is_none() {
      config.api_key = config.key_from_environment();
    }
    config.validate()?;
    Ok(config)
  }

  /// Full URL of the chat-completions endpoint.
  pub fn chat_completions_url(&self) -> String {
    let base = self.base_url.trim_end_matches('/');
    if base.ends_with("/chat/completions") {
      base.to_string()
    } else {
      format!("{base}/chat/completions")
    }
  }

  /// The credential to send, if any.
  pub fn credential(&self) -> Option<String> {
    self
      .api_key
      .clone()
      .or_else(|| self.key_from_environment())
      .filter(|value| !value.trim().is_empty())
  }

  /// Declared gaps relative to a session's needs.
  pub fn gaps(&self, required: &ModelCapabilities) -> Vec<CapabilityGap> {
    self.capabilities.gaps(required)
  }

  /// Reject configs that would surface as confusing runtime failures.
  pub fn validate(&self) -> Result<(), BuildError> {
    if self.model.trim().is_empty() {
      return Err(BuildError::Invalid("model must be set"));
    }
    let base = self.base_url.trim();
    if !(base.starts_with("http://") || base.starts_with("https://")) {
      return Err(BuildError::Invalid(
        "base_url must start with http:// or https://",
      ));
    }
    if url_has_userinfo(base) {
      return Err(BuildError::Invalid(
        "base_url must not contain userinfo credentials",
      ));
    }
    if !self.capabilities.text {
      return Err(BuildError::MissingCapability(CapabilityGap::Text));
    }
    if self.request_timeout_ms == Some(0) {
      return Err(BuildError::Invalid(
        "request_timeout_ms must be greater than zero",
      ));
    }
    Ok(())
  }

  fn key_from_environment(&self) -> Option<String> {
    self
      .api_key_env
      .as_ref()
      .and_then(|name| std::env::var(name).ok())
      .filter(|value| !value.trim().is_empty())
  }
}

pub(crate) fn redact_url(url: &str) -> String {
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

fn serialize_redacted_url<S>(url: &str, serializer: S) -> Result<S::Ok, S::Error>
where
  S: Serializer,
{
  redact_url(url).serialize(serializer)
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

fn url_has_userinfo(url: &str) -> bool {
  let Some((_, authority_and_path)) = url.split_once("://") else {
    return false;
  };
  authority_and_path
    .split_once('/')
    .map(|(authority, _)| authority.contains('@'))
    .unwrap_or_else(|| authority_and_path.contains('@'))
}

/// One pooled agent per timeout and proxy-environment profile.
///
/// `ureq::Agent` keeps a connection pool; rebuilding it per request would
/// reconnect every turn, which is the largest avoidable cost in a local
/// provider loop. The environment fingerprint prevents a changed proxy
/// configuration from reusing an agent bound to the old route.
pub(crate) fn agent_for_proxy(
  config: &ProviderConfig,
  proxy_url: &str,
  relay_nonce: &str,
) -> Result<ureq::Agent, BuildError> {
  let proxy = ureq::Proxy::new(proxy_url).map_err(|_| BuildError::Invalid("proxy URL"))?;
  Ok(
    ureq::builder()
      .timeout_connect(Duration::from_millis(config.connect_timeout_ms.max(1)))
      .timeout_read(Duration::from_millis(effective_read_timeout_ms(config)))
      .user_agent(&format!("rupi-relay/{relay_nonce}"))
      .proxy(proxy)
      .build(),
  )
}

type AgentCacheKey = (u64, u64, u64);
type AgentCache = BTreeMap<AgentCacheKey, ureq::Agent>;

pub(crate) fn agent_for(config: &ProviderConfig) -> ureq::Agent {
  static AGENTS: OnceLock<Mutex<AgentCache>> = OnceLock::new();
  let read_timeout_ms = effective_read_timeout_ms(config);
  let key = (
    config.connect_timeout_ms,
    read_timeout_ms,
    proxy_environment_fingerprint(),
  );
  let agents = AGENTS.get_or_init(|| Mutex::new(BTreeMap::new()));
  let mut agents = agents.lock().expect("provider agent cache lock poisoned");
  if let Some(agent) = agents.get(&key) {
    return agent.clone();
  }
  let agent = ureq::builder()
    .timeout_connect(Duration::from_millis(config.connect_timeout_ms.max(1)))
    .timeout_read(Duration::from_millis(read_timeout_ms))
    .try_proxy_from_env(true)
    .build();
  agents.insert(key, agent.clone());
  agent
}

/// A total request deadline must also shorten the blocking socket poll. The
/// outer worker owns the wall-clock accounting, but a socket configured with a
/// much longer idle timeout could otherwise keep teardown waiting after that
/// deadline has fired. This remains a poll, not a replacement for the outer
/// total-budget check: streamed responses are still bounded by elapsed time.
fn effective_read_timeout_ms(config: &ProviderConfig) -> u64 {
  config
    .request_timeout_ms
    .map_or(config.read_timeout_ms, |total| {
      config.read_timeout_ms.min(total)
    })
    .max(1)
}

fn proxy_environment_fingerprint() -> u64 {
  let mut hasher = std::collections::hash_map::DefaultHasher::new();
  for name in [
    "ALL_PROXY",
    "all_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "NO_PROXY",
    "no_proxy",
  ] {
    name.hash(&mut hasher);
    std::env::var_os(name).hash(&mut hasher);
  }
  hasher.finish()
}

#[cfg(test)]
mod tests {
  use super::*;

  fn config() -> ProviderConfig {
    ProviderConfig::local("local", "qwen", "http://127.0.0.1:8080/v1/", 32_768)
  }

  #[test]
  fn chat_url_is_appended_once() {
    assert_eq!(
      config().chat_completions_url(),
      "http://127.0.0.1:8080/v1/chat/completions"
    );
    let explicit = ProviderConfig {
      base_url: "https://gw.internal/v1/chat/completions".into(),
      ..config()
    };
    assert_eq!(
      explicit.chat_completions_url(),
      "https://gw.internal/v1/chat/completions"
    );
  }

  #[test]
  fn empty_model_is_rejected_at_construction() {
    let mut broken = config();
    broken.model.clear();
    assert_eq!(
      broken.validate(),
      Err(BuildError::Invalid("model must be set"))
    );
  }

  #[test]
  fn non_http_base_url_is_rejected_rather_than_mangled() {
    let mut broken = config();
    broken.base_url = "127.0.0.1:8080/v1".into();
    assert!(matches!(broken.validate(), Err(BuildError::Invalid(_))));
  }

  #[test]
  fn text_capability_is_required() {
    let mut broken = config();
    broken.capabilities.text = false;
    assert!(matches!(
      broken.validate(),
      Err(BuildError::MissingCapability(_))
    ));
  }

  #[test]
  fn endpoint_credentials_come_from_the_named_variable() {
    let endpoint = ModelEndpoint {
      api_key: None,
      api_key_env: Some("RUPI_TEST_NEVER_SET_KEY".into()),
      request_timeout_ms: Some(120_000),
      ..ModelEndpoint::local("local", "qwen", "http://127.0.0.1:8080/v1", 4_096)
    };
    let derived = ProviderConfig::from_endpoint(&endpoint).unwrap();
    assert_eq!(derived.credential(), None);
    assert_eq!(derived.max_output_tokens, None);
    assert_eq!(derived.request_timeout_ms, Some(120_000));
    assert_eq!(derived.id, "local");
  }

  #[test]
  fn zero_request_timeout_is_rejected() {
    let mut broken = config();
    broken.request_timeout_ms = Some(0);
    assert_eq!(
      broken.validate(),
      Err(BuildError::Invalid(
        "request_timeout_ms must be greater than zero"
      ))
    );
  }

  #[test]
  fn credentials_are_absent_from_serialization_and_debug() {
    let with_key = ProviderConfig {
      api_key: Some("sk-secret".into()),
      headers: BTreeMap::from([(String::from("Authorization"), String::from("Bearer secret"))]),
      ..config()
    };
    let text = serde_json::to_string(&with_key).unwrap();
    assert!(!text.contains("sk-secret"), "{text}");
    assert!(!text.contains("Bearer secret"), "{text}");
    assert!(text.contains("[redacted]"), "{text}");
    let debug = format!("{with_key:?}");
    assert!(!debug.contains("sk-secret"), "{debug}");
    assert!(!debug.contains("Bearer secret"), "{debug}");
    assert!(debug.contains("header_names"), "{debug}");
  }

  #[test]
  fn userinfo_credentials_are_rejected() {
    let mut broken = config();
    broken.base_url = "https://user:secret@example.test/v1".into();
    assert!(matches!(
      broken.validate(),
      Err(BuildError::Invalid(message)) if message.contains("userinfo")
    ));
  }

  #[test]
  fn gaps_use_the_core_capability_rule() {
    let required = ModelCapabilities {
      tools: true,
      ..ModelCapabilities::text_only(1)
    };
    let gaps = config().gaps(&required);
    assert_eq!(gaps, vec![CapabilityGap::Tools], "tools is the only gap");
  }

  #[test]
  fn remote_endpoint_defaults_to_default_base_url_and_resolves_env_key() {
    let endpoint = ModelEndpoint::remote(
      "openai",
      "gpt-4o",
      None::<String>,
      "RUPI_TEST_REMOTE_CONFIG_KEY",
      128_000,
    );
    unsafe {
      std::env::set_var("RUPI_TEST_REMOTE_CONFIG_KEY", "sk-live-test");
    }
    let derived = ProviderConfig::from_endpoint(&endpoint).unwrap();
    assert_eq!(derived.base_url, DEFAULT_BASE_URL);
    assert_eq!(
      derived.chat_completions_url(),
      "https://api.openai.com/v1/chat/completions"
    );
    assert_eq!(derived.credential().as_deref(), Some("sk-live-test"));
    unsafe {
      std::env::remove_var("RUPI_TEST_REMOTE_CONFIG_KEY");
    }
  }

  #[test]
  fn remote_config_constructor_honours_custom_base_url() {
    let cfg = ProviderConfig::remote(
      "groq",
      "llama-3.3-70b",
      Some("https://api.groq.com/openai/v1"),
      "GROQ_API_KEY",
      131_072,
    );
    assert_eq!(cfg.id, "groq");
    assert_eq!(cfg.model, "llama-3.3-70b");
    assert_eq!(cfg.base_url, "https://api.groq.com/openai/v1");
    assert_eq!(
      cfg.chat_completions_url(),
      "https://api.groq.com/openai/v1/chat/completions"
    );
    assert_eq!(cfg.api_key_env.as_deref(), Some("GROQ_API_KEY"));
  }
}
