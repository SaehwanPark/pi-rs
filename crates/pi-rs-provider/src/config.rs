//! Endpoint configuration for the OpenAI-compatible adapter.
//!
//! The knobs here exist because the "OpenAI-compatible" family is not one
//! implementation. Servers disagree on the token-limit field name, on how the
//! thinking switch is spelled, and on whether SSE is implemented correctly.
//! Each difference is expressed as one explicit, tested switch instead of a
//! guess inside the request path.

use std::{collections::BTreeMap, fmt, sync::OnceLock, time::Duration};

use pi_rs_core::{CapabilityGap, ModelCapabilities, ModelEndpoint, ReasoningExposure};
use serde::{Deserialize, Serialize};

/// Base URL used when nothing is configured.
pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// Largest error body read from a provider.
pub(crate) const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// Configuration for one OpenAI-compatible endpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderConfig {
  /// Provider id used in `provider/model` references.
  pub id: String,
  /// Human-readable name.
  pub name: String,
  /// API root, for example `http://127.0.0.1:8080/v1`.
  pub base_url: String,
  /// Model id used when a request does not name one.
  pub model: String,
  /// Literal credential. `api_key_env` is preferred; this field is never
  /// serialized back out.
  #[serde(skip_serializing)]
  pub api_key: Option<String>,
  /// Environment variable holding the credential.
  pub api_key_env: Option<String>,
  /// Extra headers, for example a gateway routing hint.
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
  pub read_timeout_ms: u64,
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
      // Reasoning generations can be silent for minutes. A shorter read
      // timeout turns slow thinking into a spurious availability failure.
      read_timeout_ms: 300_000,
    }
  }
}

/// Which token-limit field an endpoint accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensField {
  /// `max_tokens`. Accepted by OpenAI, vLLM, llama.cpp, and Ollama's OpenAI
  /// surface, so it is the default for a harness that expects local servers.
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
    if !self.capabilities.text {
      return Err(BuildError::MissingCapability(CapabilityGap::Text));
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

/// One pooled agent per timeout profile.
///
/// `ureq::Agent` keeps a connection pool; rebuilding it per request would
/// reconnect every turn, which is the largest avoidable cost in a local
/// provider loop.
pub(crate) fn agent_for(config: &ProviderConfig) -> ureq::Agent {
  static AGENTS: OnceLock<BTreeMap<(u64, u64), ureq::Agent>> = OnceLock::new();
  let key = (config.connect_timeout_ms, config.read_timeout_ms);
  let agents = AGENTS.get_or_init(BTreeMap::new);
  if let Some(agent) = agents.get(&key) {
    return agent.clone();
  }
  ureq::builder()
    .timeout_connect(Duration::from_millis(config.connect_timeout_ms.max(1)))
    .timeout_read(Duration::from_millis(config.read_timeout_ms.max(1)))
    .try_proxy_from_env(true)
    .build()
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
      api_key_env: Some("PI_RS_TEST_NEVER_SET_KEY".into()),
      ..ModelEndpoint::local("local", "qwen", "http://127.0.0.1:8080/v1", 4_096)
    };
    let derived = ProviderConfig::from_endpoint(&endpoint).unwrap();
    assert_eq!(derived.credential(), None);
    assert_eq!(derived.max_output_tokens, None);
    assert_eq!(derived.id, "local");
  }

  #[test]
  fn credential_survives_serialization_as_absent() {
    let with_key = ProviderConfig {
      api_key: Some("sk-secret".into()),
      ..config()
    };
    let text = serde_json::to_string(&with_key).unwrap();
    assert!(!text.contains("sk-secret"), "{text}");
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
}
