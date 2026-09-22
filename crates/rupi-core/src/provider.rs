//! Provider adapter boundary.
//!
//! A provider adapter's only job is normalization: model identity,
//! capabilities, streamed text, streamed reasoning **with provenance**,
//! fully-decoded tool calls, and failures. Anything above this layer must be
//! provider-agnostic, including the context engine, the session writer, and
//! the UI.
//!
//! Adapters must not decide policy. They do not retry, they do not fail over,
//! and they do not decide whether output was committed; they report facts in
//! [`crate::failure`] terms and let the runtime decide.

use std::sync::{
  Arc,
  atomic::{AtomicBool, Ordering},
};

use serde::{Deserialize, Serialize};

use crate::{
  capability::{ModelCapabilities, ModelRef},
  failure::{CompletionCertainty, ModelFailure},
  message::{Message, ToolCallBlock},
  provenance::ReasoningProvenance,
};

/// Cooperative cancellation shared between the UI and an in-flight request.
///
/// Cancellation is a request, not a guarantee: an adapter checks it between
/// stream reads and returns [`crate::failure::ModelFailureKind::Cancelled`],
/// leaving the runtime free to keep whatever output was already committed.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn cancel(&self) {
    self.0.store(true, Ordering::SeqCst);
  }

  pub fn is_cancelled(&self) -> bool {
    self.0.load(Ordering::SeqCst)
  }

  /// Borrow the underlying [`AtomicBool`] for signal-safe cancellation.
  pub fn raw_flag(&self) -> &AtomicBool {
    &self.0
  }
}

/// Requested reasoning depth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
  Off,
  Minimal,
  Low,
  #[default]
  Medium,
  High,
  Xhigh,
}

impl ThinkingLevel {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Off => "off",
      Self::Minimal => "minimal",
      Self::Low => "low",
      Self::Medium => "medium",
      Self::High => "high",
      Self::Xhigh => "xhigh",
    }
  }

  pub fn parse(value: &str) -> Option<Self> {
    match value.trim().to_ascii_lowercase().as_str() {
      "off" | "none" => Some(Self::Off),
      "minimal" => Some(Self::Minimal),
      "low" => Some(Self::Low),
      "medium" | "mid" => Some(Self::Medium),
      "high" => Some(Self::High),
      "xhigh" | "max" => Some(Self::Xhigh),
      _ => None,
    }
  }
}

/// One tool exposed to the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSpec {
  pub name: String,
  pub description: String,
  /// JSON Schema for arguments.
  pub parameters: serde_json::Value,
}

/// One model request.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRequest {
  pub model: ModelRef,
  pub capabilities: ModelCapabilities,
  pub system: Option<String>,
  pub messages: Vec<Message>,
  pub tools: Vec<ToolSpec>,
  pub max_output_tokens: Option<u64>,
  pub temperature: Option<f32>,
  pub thinking: ThinkingLevel,
  pub stop: Vec<String>,
}

impl ModelRequest {
  pub fn new(model: ModelRef, capabilities: ModelCapabilities, messages: Vec<Message>) -> Self {
    Self {
      model,
      capabilities,
      system: None,
      messages,
      tools: Vec::new(),
      max_output_tokens: None,
      temperature: None,
      thinking: ThinkingLevel::default(),
      stop: Vec::new(),
    }
  }

  pub fn with_system(mut self, system: impl Into<String>) -> Self {
    self.system = Some(system.into());
    self
  }

  pub fn with_tools(mut self, tools: Vec<ToolSpec>) -> Self {
    self.tools = tools;
    self
  }

  pub fn with_thinking(mut self, level: ThinkingLevel) -> Self {
    self.thinking = level;
    self
  }

  /// Rough token estimate used only before a provider reports real usage.
  ///
  /// Four bytes per token underestimates CJK and code-heavy text and
  /// overestimates prose; that is acceptable for an estimate that only drives
  /// context thresholds, and is never presented as a measurement.
  pub fn estimate_tokens(&self) -> u64 {
    let system = self.system.as_deref().unwrap_or("");
    let tools: usize = self
      .tools
      .iter()
      .map(|tool| tool.name.len() + tool.description.len() + tool.parameters.to_string().len())
      .sum();
    let messages: usize = self
      .messages
      .iter()
      .map(|message| {
        message
          .content
          .iter()
          .map(|block| serde_json::to_string(block).map(|s| s.len()).unwrap_or(0))
          .sum::<usize>()
      })
      .sum();
    ((system.len() + tools + messages) as u64 / 4) + 1
  }
}

/// Usage and finish information for one completed request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionUsage {
  /// Logical prompt tokens, retained under the original field name for callers
  /// that predate cache-aware accounting.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub input_tokens: Option<u64>,
  /// Prompt tokens excluding cache reads and cache writes; cache writes are tracked separately.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub uncached_input_tokens: Option<u64>,
  /// Logical prompt footprint, including cached and write-through prompt tokens.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub logical_prompt_tokens: Option<u64>,
  /// Prompt tokens served from the provider's cache.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub cache_read_tokens: Option<u64>,
  /// Prompt tokens written to the provider's cache during this request.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub cache_write_tokens: Option<u64>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub output_tokens: Option<u64>,
  /// Provider-reported total, when present, including cache accounting.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub provider_total_tokens: Option<u64>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub finish_reason: Option<String>,
  /// Whether the adapter actually observed the end of the response.
  ///
  /// Usage counts are optional and a missing count is normal; an *unobserved*
  /// completion is not. Without this field the trait signature would silently turn
  /// "the stream stopped mid-sentence" into "the turn succeeded", and a runtime
  /// cannot recover a distinction its contract erased. Defaults to
  /// [`CompletionCertainty::Certain`] so traces written before the field existed
  /// still load, and loading them means trusting what they claimed.
  #[serde(default = "certain")]
  pub certainty: CompletionCertainty,
}

fn certain() -> CompletionCertainty {
  CompletionCertainty::Certain
}

impl CompletionUsage {
  pub fn unknown() -> Self {
    Self {
      input_tokens: None,
      uncached_input_tokens: None,
      logical_prompt_tokens: None,
      cache_read_tokens: None,
      cache_write_tokens: None,
      output_tokens: None,
      provider_total_tokens: None,
      finish_reason: None,
      certainty: CompletionCertainty::Certain,
    }
  }

  /// A response whose completion was never observed.
  ///
  /// This is the shape an adapter returns when the transport ended cleanly enough
  /// to count tokens but never said the turn was over. It is not a failure by
  /// itself; it is the runtime's cue to decide what an unfinished answer means.
  pub fn unfinished() -> Self {
    Self {
      input_tokens: None,
      uncached_input_tokens: None,
      logical_prompt_tokens: None,
      cache_read_tokens: None,
      cache_write_tokens: None,
      output_tokens: None,
      provider_total_tokens: None,
      finish_reason: None,
      certainty: CompletionCertainty::Unknown,
    }
  }

  /// `true` when the adapter saw a definitive end of response.
  pub fn is_certain(&self) -> bool {
    self.certainty == CompletionCertainty::Certain
  }

  /// `true` when the provider stopped because the configured output budget was
  /// reached rather than because it produced a complete answer.
  ///
  /// OpenAI-compatible endpoints conventionally report `length`; a few local
  /// adapters use the more literal `max_tokens` label. Both are observed
  /// completion boundaries, but neither is a usable final answer for a coding
  /// turn. Keeping this predicate on the shared usage type prevents provider
  /// adapters and the runtime from silently assigning different meanings to the
  /// same finish reason.
  pub fn stopped_at_output_limit(&self) -> bool {
    matches!(self.finish_reason.as_deref(), Some("length" | "max_tokens"))
  }
}

/// One normalized streaming fact from a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderEvent {
  /// Reasoning-like text. The provenance claim is mandatory, which is why this
  /// variant cannot carry a bare string.
  ReasoningDelta {
    text: String,
    provenance: ReasoningProvenance,
  },
  TextDelta(String),
  /// A complete, decoded tool call. Partial tool-call fragments stay inside
  /// the adapter.
  ToolCall(ToolCallBlock),
  /// A complete model-authored tool call that cannot safely be dispatched.
  ///
  /// Adapters use this for malformed arguments or ambiguous fragment
  /// correlation. The runtime records a failed tool result so the model can
  /// correct its invocation; it must never execute this call.
  ToolCallRejected {
    id: crate::ToolCallId,
    name: String,
    reason: String,
  },
}

/// Sink for provider output while a request streams.
pub trait ProviderEventSink: Send {
  fn emit(&mut self, event: &ProviderEvent);
}

/// In-order event collector.
///
/// Adapters use it internally when a stream boundary must be respected (for
/// example while tool-call argument fragments accumulate), and tests use it as
/// the observable stream. It intentionally keeps provenance attached instead of
/// flattening reasoning into a string.
#[derive(Default)]
pub struct Collector {
  events: Vec<ProviderEvent>,
}

impl Collector {
  pub fn events(&self) -> &[ProviderEvent] {
    &self.events
  }

  pub fn into_events(self) -> Vec<ProviderEvent> {
    self.events
  }

  /// Concatenated visible text deltas.
  pub fn text(&self) -> String {
    self
      .events
      .iter()
      .filter_map(|event| match event {
        ProviderEvent::TextDelta(text) => Some(text.as_str()),
        _ => None,
      })
      .collect()
  }

  /// Concatenated reasoning text per provenance claim, so that two claims are
  /// never merged into one anonymous buffer.
  pub fn reasoning(&self) -> Vec<(ReasoningProvenance, String)> {
    let mut out: Vec<(ReasoningProvenance, String)> = Vec::new();
    for event in &self.events {
      if let ProviderEvent::ReasoningDelta { text, provenance } = event {
        if let Some(entry) = out.iter_mut().find(|(p, _)| p == provenance) {
          entry.1.push_str(text);
        } else {
          out.push((*provenance, text.clone()));
        }
      }
    }
    out
  }
}

impl ProviderEventSink for Collector {
  fn emit(&mut self, event: &ProviderEvent) {
    self.events.push(event.clone());
  }
}

/// One configured model.
pub trait ModelProvider: Send + Sync {
  /// Configuration provider id, for example `local-vulkan`.
  fn provider_id(&self) -> &str;

  fn model(&self) -> &ModelRef;

  /// Declared capabilities. Called to build the epoch snapshot and to gate
  /// failover; must not require network access.
  fn capabilities(&self) -> ModelCapabilities;

  /// Re-arm an adapter for a new user turn after the previous turn abandoned
  /// an in-flight request. The default is a no-op; adapters that quarantine an
  /// uncertain transport may open a fresh request generation here. Runtime
  /// retries within the same turn never call this hook.
  fn reset_after_abandonment(&self) {}

  /// Stream one completion.
  ///
  /// The failure object is deliberately by-value rather than boxed: it is
  /// constructed on cold paths only, and every consumer matches on it, so a
  /// layer of indirection would cost readability without protecting a hot
  /// path.
  #[allow(clippy::result_large_err)]
  /// Implementations must:
  ///
  /// - return [`crate::failure::ModelFailureKind::Cancelled`] promptly when
  ///   `cancel` is set;
  /// - classify every failure through [`ModelFailure`], including mid-stream
  ///   interruptions, with `phase` and `partial_output_emitted` set honestly;
  /// - never invent reasoning text or provenance.
  fn stream(
    &self,
    request: &ModelRequest,
    sink: &mut dyn ProviderEventSink,
    cancel: &CancelToken,
  ) -> Result<CompletionUsage, ModelFailure>;
}

#[cfg(test)]
mod tests {
  use crate::{
    capability::ReasoningExposure,
    failure::{FailurePhase, ModelFailureKind},
  };

  use super::*;

  struct EchoProvider;

  impl ModelProvider for EchoProvider {
    fn provider_id(&self) -> &str {
      "echo"
    }

    fn model(&self) -> &ModelRef {
      static MODEL: std::sync::OnceLock<ModelRef> = std::sync::OnceLock::new();
      MODEL.get_or_init(|| ModelRef::new("echo", "echo"))
    }

    fn capabilities(&self) -> ModelCapabilities {
      ModelCapabilities {
        tools: true,
        exposed_reasoning: ReasoningExposure::Native,
        ..ModelCapabilities::text_only(8_000)
      }
    }

    fn stream(
      &self,
      request: &ModelRequest,
      sink: &mut dyn ProviderEventSink,
      cancel: &CancelToken,
    ) -> Result<CompletionUsage, ModelFailure> {
      if cancel.is_cancelled() {
        return Err(ModelFailure::new(
          ModelFailureKind::Cancelled,
          FailurePhase::PreRequest,
          "cancelled before send",
        ));
      }
      sink.emit(&ProviderEvent::ReasoningDelta {
        text: "echoing the last user message".into(),
        provenance: ReasoningProvenance::Native,
      });
      let text = request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == crate::message::Role::User)
        .map(|message| message.text())
        .unwrap_or_else(|| "(nothing)".to_string());
      // Two deltas on purpose: consumers must handle streamed text.
      sink.emit(&ProviderEvent::TextDelta("echo: ".into()));
      sink.emit(&ProviderEvent::TextDelta(text));
      Ok(CompletionUsage {
        input_tokens: Some(request.estimate_tokens()),
        uncached_input_tokens: Some(request.estimate_tokens()),
        logical_prompt_tokens: Some(request.estimate_tokens()),
        cache_read_tokens: None,
        cache_write_tokens: None,
        output_tokens: Some(4),
        provider_total_tokens: Some(request.estimate_tokens() + 4),
        finish_reason: Some("stop".into()),
        certainty: CompletionCertainty::Certain,
      })
    }
  }

  #[test]
  fn adapter_streams_text_and_provenanced_reasoning() {
    let provider = EchoProvider;
    let mut collector = Collector::default();
    let usage = provider
      .stream(
        &ModelRequest::new(
          provider.model().clone(),
          provider.capabilities(),
          vec![Message::user("hello")],
        ),
        &mut collector,
        &CancelToken::new(),
      )
      .unwrap();
    assert_eq!(
      collector.events(),
      &[
        ProviderEvent::ReasoningDelta {
          text: "echoing the last user message".into(),
          provenance: ReasoningProvenance::Native,
        },
        ProviderEvent::TextDelta("echo: ".into()),
        ProviderEvent::TextDelta("hello".into()),
      ]
    );
    assert_eq!(collector.text(), "echo: hello");
    assert_eq!(
      collector.reasoning(),
      vec![(
        ReasoningProvenance::Native,
        "echoing the last user message".into()
      )]
    );
    assert_eq!(usage.finish_reason.as_deref(), Some("stop"));
    assert!(!usage.stopped_at_output_limit());
  }

  #[test]
  fn output_limit_finish_reasons_are_not_usable_answers() {
    for reason in ["length", "max_tokens"] {
      let usage = CompletionUsage {
        input_tokens: None,
        uncached_input_tokens: None,
        logical_prompt_tokens: None,
        cache_read_tokens: None,
        cache_write_tokens: None,
        output_tokens: Some(8_192),
        provider_total_tokens: None,
        finish_reason: Some(reason.into()),
        certainty: CompletionCertainty::Certain,
      };
      assert!(usage.stopped_at_output_limit(), "{reason}");
    }
    let normal = CompletionUsage {
      input_tokens: None,
      uncached_input_tokens: None,
      logical_prompt_tokens: None,
      cache_read_tokens: None,
      cache_write_tokens: None,
      output_tokens: Some(4),
      provider_total_tokens: None,
      finish_reason: Some("stop".into()),
      certainty: CompletionCertainty::Certain,
    };
    assert!(!normal.stopped_at_output_limit());
  }

  #[test]
  fn cancellation_is_reported_not_swallowed() {
    let provider = EchoProvider;
    let cancel = CancelToken::new();
    cancel.cancel();
    let failure = provider
      .stream(
        &ModelRequest::new(
          provider.model().clone(),
          provider.capabilities(),
          vec![Message::user("hello")],
        ),
        &mut Collector::default(),
        &cancel,
      )
      .unwrap_err();
    assert_eq!(failure.kind, ModelFailureKind::Cancelled);
    assert_eq!(failure.phase, FailurePhase::PreRequest);
  }

  #[test]
  fn token_estimate_grows_with_content() {
    let small = ModelRequest::new(
      ModelRef::new("local", "m"),
      ModelCapabilities::text_only(100),
      vec![Message::user("short")],
    );
    let large = ModelRequest::new(
      ModelRef::new("local", "m"),
      ModelCapabilities::text_only(100),
      vec![Message::user("long content ".repeat(200))],
    );
    assert!(small.estimate_tokens() > 0);
    assert!(large.estimate_tokens() > small.estimate_tokens() * 10);

    let with_tools = small.clone().with_tools(vec![ToolSpec {
      name: "read".into(),
      description: "read a file".into(),
      parameters: serde_json::json!({"type": "object"}),
    }]);
    assert!(with_tools.estimate_tokens() > small.estimate_tokens());
  }

  #[test]
  fn thinking_levels_parse_the_documented_names() {
    for level in [
      ThinkingLevel::Off,
      ThinkingLevel::Minimal,
      ThinkingLevel::Low,
      ThinkingLevel::Medium,
      ThinkingLevel::High,
      ThinkingLevel::Xhigh,
    ] {
      assert_eq!(ThinkingLevel::parse(level.as_str()), Some(level));
    }
    assert_eq!(ThinkingLevel::parse("none"), Some(ThinkingLevel::Off));
    assert_eq!(ThinkingLevel::parse("max"), Some(ThinkingLevel::Xhigh));
    assert_eq!(ThinkingLevel::parse("very-high"), None);
  }

  #[test]
  fn tool_call_events_are_complete_blocks() {
    let event = ProviderEvent::ToolCall(ToolCallBlock {
      id: crate::ids::ToolCallId::new(),
      name: "read".into(),
      arguments: serde_json::json!({ "path": "src/main.rs" }),
    });
    let ProviderEvent::ToolCall(call) = event else {
      panic!("wrong variant");
    };
    assert_eq!(
      call.arguments.get("path").and_then(|v| v.as_str()),
      Some("src/main.rs")
    );
  }
}
