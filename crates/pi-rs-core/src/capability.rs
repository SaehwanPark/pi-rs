//! Model identity and capability claims.
//!
//! Capabilities are what the runtime is allowed to rely on. They are declared
//! per model, snapshotted into every model epoch, and compared before a backup
//! model takes over. Absent information is treated as unsupported: an
//! unproven modality must not be used, and an unproven context window must not
//! be trusted.

use serde::{Deserialize, Serialize};

use crate::ids::{SessionId, TurnId};

/// Provider-qualified model identity.
///
/// Rendered as `provider/model`, which is also the shape used by configuration
/// (`primary = "local/qwen"`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModelRef {
  pub provider: String,
  pub model: String,
}

impl Serialize for ModelRef {
  fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&self.as_key())
  }
}

impl<'de> Deserialize<'de> for ModelRef {
  fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
    struct ModelRefVisitor;

    impl serde::de::Visitor<'_> for ModelRefVisitor {
      type Value = ModelRef;

      fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a model reference formatted as provider/model")
      }

      fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
        ModelRef::parse(value)
          .ok_or_else(|| E::invalid_value(serde::de::Unexpected::Str(value), &self))
      }
    }

    deserializer.deserialize_str(ModelRefVisitor)
  }
}

impl ModelRef {
  pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
    Self {
      provider: provider.into(),
      model: model.into(),
    }
  }

  /// Parse `provider/model`. Both halves are required so that a bare model
  /// name cannot silently pick a default provider.
  pub fn parse(value: &str) -> Option<Self> {
    let (provider, model) = value.split_once('/')?;
    if provider.trim().is_empty() || model.trim().is_empty() {
      return None;
    }
    Some(Self::new(provider.trim(), model.trim()))
  }

  pub fn as_key(&self) -> String {
    format!("{}/{}", self.provider, self.model)
  }
}

impl std::fmt::Display for ModelRef {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "{}/{}", self.provider, self.model)
  }
}

/// Exposure style for reasoning-like output from a model/provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningExposure {
  /// No reasoning-like output is available.
  None,
  /// Native reasoning content is streamed or returned verbatim.
  Native,
  /// Only a provider-authored summary is available.
  ProviderSummary,
  /// Reasoning-like text appears only because the runtime asked for it.
  Declared,
}

impl ReasoningExposure {
  /// Provenance implied by this exposure when text actually arrives.
  ///
  /// Returns `None` for [`ReasoningExposure::None`] so that a model which is
  /// not expected to expose reasoning cannot accidentally produce `Native`
  /// reasoning by default.
  pub fn implied_provenance(self) -> Option<crate::provenance::ReasoningProvenance> {
    use crate::provenance::ReasoningProvenance as P;
    match self {
      Self::None => None,
      Self::Native => Some(P::Native),
      Self::ProviderSummary => Some(P::ProviderSummary),
      Self::Declared => Some(P::Declared),
    }
  }
}

/// What a model is claimed to be able to do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCapabilities {
  pub text: bool,
  pub images: bool,
  pub tools: bool,
  pub exposed_reasoning: ReasoningExposure,
  /// Largest total input the provider is believed to accept, in tokens.
  pub context_window: u64,
  /// Provider-side output ceiling, when the provider declares one.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub max_output_tokens: Option<u64>,
}

impl ModelCapabilities {
  /// Text-only baseline with an explicit small window.
  ///
  /// The window is not invented from a provider advertisement: callers must
  /// overwrite it when they have a real number, and the context policy refuses
  /// to scale thresholds upward on guesses.
  pub fn text_only(context_window: u64) -> Self {
    Self {
      text: true,
      images: false,
      tools: false,
      exposed_reasoning: ReasoningExposure::None,
      context_window,
      max_output_tokens: None,
    }
  }

  /// Capabilities that `self` lacks relative to what the session needs.
  ///
  /// Used by the backup compatibility gate. Context window shortfall is
  /// reported separately from hard modality gaps because it is recoverable by
  /// compaction rather than by refusal.
  pub fn gaps(&self, required: &ModelCapabilities) -> Vec<CapabilityGap> {
    let mut gaps = Vec::new();
    if required.text && !self.text {
      gaps.push(CapabilityGap::Text);
    }
    if required.images && !self.images {
      gaps.push(CapabilityGap::Images);
    }
    if required.tools && !self.tools {
      gaps.push(CapabilityGap::Tools);
    }
    if self.context_window < required.context_window {
      gaps.push(CapabilityGap::ContextWindow {
        required: required.context_window,
        available: self.context_window,
      });
    }
    gaps
  }

  /// `true` when no gap can be closed by compaction, i.e. the takeover is
  /// impossible rather than merely expensive.
  pub fn has_hard_gap(gaps: &[CapabilityGap]) -> bool {
    gaps
      .iter()
      .any(|gap| !matches!(gap, CapabilityGap::ContextWindow { .. }))
  }
}

/// One specific capability shortfall between a required and an available
/// capability snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum CapabilityGap {
  Text,
  Images,
  Tools,
  ContextWindow { required: u64, available: u64 },
}

impl std::fmt::Display for CapabilityGap {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Text => f.write_str("text input"),
      Self::Images => f.write_str("image input"),
      Self::Tools => f.write_str("tool calling"),
      Self::ContextWindow {
        required,
        available,
      } => write!(f, "context window {available} < required {required}"),
    }
  }
}

/// Which model currently owns generation, with the snapshot that was validated
/// when it became active.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelEpoch {
  /// Monotonic index inside one session. Epoch 0 is the initial epoch.
  pub index: u32,
  pub model: ModelRef,
  pub capabilities: ModelCapabilities,
  pub reason: EpochReason,
  /// Event that opened this epoch, for trace attribution.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub started_by_event: Option<crate::ids::EventId>,
}

/// Why a model epoch began.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpochReason {
  Initial,
  ManualSwitch,
  AutomaticFailover,
  /// Switch back after an automatic takeover, always user-initiated.
  ManualSwitchBack,
}

impl EpochReason {
  pub fn is_automatic(self) -> bool {
    matches!(self, Self::AutomaticFailover)
  }
}

/// Attribution carried by anything that must remain answerable as "which model
/// produced this".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelAttribution {
  pub session_id: SessionId,
  pub turn_id: TurnId,
  pub epoch: u32,
  pub model: ModelRef,
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn model_ref_parse_requires_both_halves() {
    assert_eq!(
      ModelRef::parse("local/qwen").unwrap().as_key(),
      "local/qwen".to_string()
    );
    assert!(ModelRef::parse("qwen").is_none());
    assert!(ModelRef::parse("local/").is_none());
  }

  #[test]
  fn gaps_separate_compactable_from_impossible() {
    let required = ModelCapabilities {
      images: true,
      tools: true,
      context_window: 64_000,
      ..ModelCapabilities::text_only(32_000)
    };
    let smaller = ModelCapabilities::text_only(16_000);
    let gaps = smaller.gaps(&required);
    assert!(gaps.contains(&CapabilityGap::Images));
    assert!(gaps.contains(&CapabilityGap::Tools));
    assert!(gaps.contains(&CapabilityGap::ContextWindow {
      required: 64_000,
      available: 16_000
    }));
    assert!(ModelCapabilities::has_hard_gap(&gaps));

    let capable = ModelCapabilities {
      context_window: 8_000,
      ..required.clone()
    };
    let gaps = capable.gaps(&required);
    assert_eq!(
      gaps,
      vec![CapabilityGap::ContextWindow {
        required: 64_000,
        available: 8_000
      }]
    );
    assert!(!ModelCapabilities::has_hard_gap(&gaps));
  }

  #[test]
  fn unknown_exposure_has_no_default_provenance() {
    assert_eq!(ReasoningExposure::None.implied_provenance(), None);
    assert_eq!(
      ReasoningExposure::ProviderSummary.implied_provenance(),
      Some(crate::provenance::ReasoningProvenance::ProviderSummary)
    );
  }
}
