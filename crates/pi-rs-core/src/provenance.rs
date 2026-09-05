//! Reasoning provenance.
//!
//! Reasoning-like text is only ever meaningful together with the claim about
//! where it came from. Four sources are distinguishable, and no layer may
//! collapse them:
//!
//! - [`ReasoningProvenance::Native`]: text the model or provider actually
//!   emitted as reasoning/thinking content.
//! - [`ReasoningProvenance::ProviderSummary`]: provider-generated summary of
//!   hidden reasoning. It is real provider output, but it is not the model's
//!   reasoning.
//! - [`ReasoningProvenance::Declared`]: explanation the runtime explicitly
//!   asked the model to produce.
//! - [`ReasoningProvenance::Reconstructed`]: post-hoc inference produced by
//!   pi-rs or an analyser. It is a hypothesis, never recovered thought.
//!
//! There is deliberately no `Default`: every producer must state its
//! provenance.

use serde::{Deserialize, Serialize};

/// Where a piece of reasoning-like text came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningProvenance {
  /// Emitted by the model/provider as reasoning content.
  Native,
  /// Provider-authored summary of reasoning the runtime cannot see.
  ProviderSummary,
  /// Explanation produced because the runtime asked for it.
  Declared,
  /// Inference made after the fact from visible evidence.
  Reconstructed,
}

impl ReasoningProvenance {
  /// Stable machine label used in trace, exports, and UI tags.
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Native => "native",
      Self::ProviderSummary => "provider_summary",
      Self::Declared => "declared",
      Self::Reconstructed => "reconstructed",
    }
  }

  /// Human label for the UI provenance grammar.
  ///
  /// The labels are intentionally different from each other so that a reader
  /// cannot mistake inference for emitted reasoning.
  pub fn label(self) -> &'static str {
    match self {
      Self::Native => "reasoning",
      Self::ProviderSummary => "provider summary",
      Self::Declared => "declared rationale",
      Self::Reconstructed => "reconstructed rationale",
    }
  }

  /// `true` for text that pi-rs produced by inference rather than receiving.
  ///
  /// `Declared` counts as authored output, not as inference: the model wrote
  /// it, even though the runtime chose to request it.
  pub fn is_inferred(self) -> bool {
    matches!(self, Self::Reconstructed)
  }

  /// `true` when the text must never be described as recovered chain of
  /// thought, which is the case for everything except native reasoning.
  pub fn is_not_hidden_reasoning(self) -> bool {
    !matches!(self, Self::Native)
  }
}

impl std::fmt::Display for ReasoningProvenance {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.label())
  }
}

/// One reasoning-like passage plus the claim about its origin.
///
/// Carried inside messages so that provenance survives session and trace
/// serialization; a reasoning string without this wrapper is not allowed to
/// cross a boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningChunk {
  pub text: String,
  pub provenance: ReasoningProvenance,
  /// Free-form origin detail, for example `openai/response.summary` or
  /// `llama.cpp/reasoning_content`. Optional because native reasoning from an
  /// unknown provider still has a valid, if coarse, provenance claim.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub source: Option<String>,
}

impl ReasoningChunk {
  pub fn new(text: impl Into<String>, provenance: ReasoningProvenance) -> Self {
    Self {
      text: text.into(),
      provenance,
      source: None,
    }
  }

  pub fn with_source(mut self, source: impl Into<String>) -> Self {
    self.source = Some(source.into());
    self
  }

  pub fn is_empty(&self) -> bool {
    self.text.is_empty()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn provenance_serializes_stably() {
    let chunk = ReasoningChunk::new("checked the failing test", ReasoningProvenance::Native)
      .with_source("llama.cpp/reasoning_content");
    let encoded = serde_json::to_string(&chunk).unwrap();
    assert!(encoded.contains("\"provenance\":\"native\""), "{encoded}");
    assert!(encoded.contains("llama.cpp/reasoning_content"), "{encoded}");
    let decoded: ReasoningChunk = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, chunk);
  }

  #[test]
  fn labels_are_distinct_and_inference_is_labeled() {
    let labels = [
      ReasoningProvenance::Native.label(),
      ReasoningProvenance::ProviderSummary.label(),
      ReasoningProvenance::Declared.label(),
      ReasoningProvenance::Reconstructed.label(),
    ];
    let mut sorted = labels.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
      sorted.len(),
      4,
      "provenance labels must stay distinguishable"
    );
    assert!(ReasoningProvenance::Reconstructed.is_inferred());
    assert!(!ReasoningProvenance::Native.is_inferred());
    assert!(ReasoningProvenance::ProviderSummary.is_not_hidden_reasoning());
  }
}
