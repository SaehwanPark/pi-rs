//! Normalized conversation content.
//!
//! This is the runtime's own representation. Provider payloads are decoded
//! into it, session state is written from it, and the context engine renders
//! it back into provider-specific request bodies. Nothing outside a provider
//! adapter may depend on a specific provider's message shape.
//!
//! Two rules are enforced by the shape of these types:
//!
//! - reasoning-like text always travels as a [`ReasoningChunk`], so its
//!   provenance cannot be lost while moving between session, context, and
//!   provider layers;
//! - a tool result always names the tool call it answers and the lifecycle
//!   state that produced it, so an uncertain side effect cannot be presented
//!   as a clean success.

use serde::{Deserialize, Serialize};

use crate::{ids::ToolCallId, provenance::ReasoningChunk, tool::ToolExecutionState};

/// Author of a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
  System,
  User,
  Assistant,
  /// Tool output, addressed back to one tool call.
  Tool,
}

impl Role {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::System => "system",
      Self::User => "user",
      Self::Assistant => "assistant",
      Self::Tool => "tool",
    }
  }
}

/// A tool invocation requested by the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallBlock {
  pub id: ToolCallId,
  pub name: String,
  /// Decoded arguments. Adapters must fully decode before this point; a
  /// partially decoded tool call is a protocol failure, not a tool call.
  pub arguments: serde_json::Value,
}

/// The runtime's answer to one tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultBlock {
  pub id: ToolCallId,
  pub name: String,
  pub state: ToolExecutionState,
  pub text: String,
  /// `true` when the tool ran but reported failure.
  #[serde(default)]
  pub is_error: bool,
  /// `true` when `text` is a bounded representation and the full payload lives
  /// in the trace store. The recovery reference is kept in trace, never
  /// invented here.
  #[serde(default)]
  pub reduced: bool,
}

/// One piece of message content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ContentBlock {
  Text {
    text: String,
  },
  Image {
    mime: String,
    /// Base64 payload. Kept inline in session state, but the trace store may
    /// replace it with a content-addressed blob reference.
    data_base64: String,
  },
  /// Reasoning or reasoning-like text with its provenance claim attached.
  Reasoning(ReasoningChunk),
  ToolCall(ToolCallBlock),
  ToolResult(ToolResultBlock),
}

impl ContentBlock {
  pub fn text(text: impl Into<String>) -> Self {
    Self::Text { text: text.into() }
  }

  pub fn plain_text(&self) -> Option<&str> {
    match self {
      Self::Text { text } => Some(text),
      _ => None,
    }
  }
}

/// One message in the canonical session record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
  pub role: Role,
  pub content: Vec<ContentBlock>,
}

impl Message {
  pub fn new(role: Role, content: Vec<ContentBlock>) -> Self {
    Self { role, content }
  }

  pub fn system(text: impl Into<String>) -> Self {
    Self::new(Role::System, vec![ContentBlock::text(text)])
  }

  pub fn user(text: impl Into<String>) -> Self {
    Self::new(Role::User, vec![ContentBlock::text(text)])
  }

  pub fn assistant(text: impl Into<String>) -> Self {
    Self::new(Role::Assistant, vec![ContentBlock::text(text)])
  }

  /// Concatenated plain text. Reasoning, tool calls, and tool results are
  /// deliberately excluded: they are not assistant prose.
  pub fn text(&self) -> String {
    self
      .content
      .iter()
      .filter_map(ContentBlock::plain_text)
      .collect::<Vec<_>>()
      .join("")
  }

  pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCallBlock> {
    self.content.iter().filter_map(|block| match block {
      ContentBlock::ToolCall(call) => Some(call),
      _ => None,
    })
  }

  pub fn reasoning(&self) -> impl Iterator<Item = &ReasoningChunk> {
    self.content.iter().filter_map(|block| match block {
      ContentBlock::Reasoning(chunk) => Some(chunk),
      _ => None,
    })
  }

  pub fn is_empty(&self) -> bool {
    self.content.is_empty()
      || self.content.iter().all(|block| match block {
        ContentBlock::Text { text } => text.is_empty(),
        _ => false,
      })
  }
}

#[cfg(test)]
mod tests {
  use crate::provenance::ReasoningProvenance;

  use super::*;

  #[test]
  fn text_ignores_reasoning_and_tool_blocks() {
    let message = Message::new(
      Role::Assistant,
      vec![
        ContentBlock::Reasoning(ReasoningChunk::new(
          "reading the failing test first",
          ReasoningProvenance::Native,
        )),
        ContentBlock::text("fixed "),
        ContentBlock::text("it"),
      ],
    );
    assert_eq!(message.text(), "fixed it");
    assert_eq!(message.reasoning().count(), 1);
  }

  #[test]
  fn provenance_survives_serialization() {
    let message = Message::new(
      Role::Assistant,
      vec![ContentBlock::Reasoning(ReasoningChunk::new(
        "maybe the provider summary",
        ReasoningProvenance::ProviderSummary,
      ))],
    );
    let encoded = serde_json::to_string(&message).unwrap();
    let decoded: Message = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, message);
    assert!(
      encoded.contains("provider_summary"),
      "provenance must not be flattened away: {encoded}"
    );
  }

  #[test]
  fn tool_result_records_state() {
    let block = ContentBlock::ToolResult(ToolResultBlock {
      id: ToolCallId::new(),
      name: "write".into(),
      state: ToolExecutionState::Unknown,
      text: "completion not observed".into(),
      is_error: false,
      reduced: false,
    });
    let encoded = serde_json::to_string(&block).unwrap();
    assert!(encoded.contains("\"state\":\"unknown\""), "{encoded}");
  }
}
