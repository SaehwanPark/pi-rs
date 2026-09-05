//! Request mapping: the harness request model to a chat-completions body.
//!
//! This lives apart from transport so the mapping is testable without a socket.
//! In practice almost every provider-adapter bug found in this class of code is
//! a mapping bug: a tool call that was never replayed, an image dropped in
//! silence, or thinking requested from a server that rejects the field.

use pi_rs_core::{ContentBlock, Message, ModelRef, ModelRequest, Role, ThinkingLevel};
use serde_json::{Value, json};

use crate::config::{MaxTokensField, ProviderConfig, ThinkingInput};

/// Build a chat-completions body.
pub fn request_body(config: &ProviderConfig, request: &ModelRequest) -> Value {
  let model = if request.model.model.trim().is_empty() {
    config.model.clone()
  } else {
    request.model.model.clone()
  };
  let mut body = json!({
    "model": model,
    "messages": messages(config, &request.system, &request.messages),
    "stream": config.stream,
  });

  if !request.tools.is_empty() && request.capabilities.tools {
    body["tools"] = Value::Array(
      request
        .tools
        .iter()
        .map(|spec| {
          json!({
            "type": "function",
            "function": {
              "name": spec.name,
              "description": spec.description,
              "parameters": spec.parameters,
            }
          })
        })
        .collect(),
    );
    body["tool_choice"] = json!("auto");
  }

  if let Some(limit) = request
    .max_output_tokens
    .or(config.max_output_tokens)
    .or(request.capabilities.max_output_tokens)
  {
    body[match config.max_tokens_field {
      MaxTokensField::MaxTokens => "max_tokens",
      MaxTokensField::MaxCompletionTokens => "max_completion_tokens",
    }] = json!(limit);
  }
  if let Some(temperature) = request.temperature {
    body["temperature"] = json!(temperature);
  }
  if !request.stop.is_empty() {
    body["stop"] = json!(request.stop);
  }
  apply_thinking(config, request.thinking, &mut body);
  if config.stream {
    body["stream_options"] = json!({ "include_usage": true });
  }
  body
}

/// Translate the thinking level into whatever dialect this endpoint expects.
fn apply_thinking(config: &ProviderConfig, level: ThinkingLevel, body: &mut Value) {
  match config.thinking_input {
    ThinkingInput::None => {}
    ThinkingInput::ReasoningEffort => {
      if level == ThinkingLevel::Off {
        return;
      }
      body["reasoning_effort"] = json!(match level {
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        // `xhigh` is outside the OpenAI enum. Clamping upward preserves the
        // intent (more thinking) instead of silently downgrading it.
        ThinkingLevel::High | ThinkingLevel::Xhigh => "high",
        ThinkingLevel::Off => unreachable!("handled above"),
      });
    }
    ThinkingInput::ChatTemplateThinking => {
      body["chat_template_kwargs"] = json!({ "thinking": level != ThinkingLevel::Off });
    }
  }
}

fn messages(config: &ProviderConfig, system: &Option<String>, history: &[Message]) -> Vec<Value> {
  let mut out = Vec::with_capacity(history.len() + 1);
  if let Some(system) = system.as_ref().filter(|text| !text.trim().is_empty()) {
    out.push(json!({ "role": "system", "content": system }));
  }
  out.extend(
    history
      .iter()
      .filter_map(|message| message_json(config, message)),
  );
  out
}

/// One message, or `None` when it carries nothing a provider can use.
fn message_json(config: &ProviderConfig, message: &Message) -> Option<Value> {
  let mut text = String::new();
  let mut images: Vec<Value> = Vec::new();
  let mut tool_calls: Vec<Value> = Vec::new();
  let mut tool_result: Option<(String, String)> = None;
  for block in &message.content {
    match block {
      ContentBlock::Text { text: chunk } => append(&mut text, chunk),
      ContentBlock::Image { mime, data_base64 } => {
        if config.capabilities.images {
          images.push(json!({
            "type": "image_url",
            "image_url": { "url": format!("data:{mime};base64,{data_base64}") },
          }));
        } else {
          // Dropping content silently makes the model answer a question it
          // cannot see. State the omission instead.
          append(
            &mut text,
            &format!("[image omitted: {mime}, no image capability]"),
          );
        }
      }
      // Reasoning is never replayed: it records a previous generation, and
      // resending it changes behaviour that nobody chose.
      ContentBlock::Reasoning(_) => {}
      ContentBlock::ToolCall(call) => tool_calls.push(json!({
        "id": call.id.as_str(),
        "type": "function",
        "function": {
          "name": call.name,
          "arguments": call.arguments.to_string(),
        },
      })),
      ContentBlock::ToolResult(result) => {
        tool_result = Some((result.id.as_str().to_string(), result.text.clone()));
      }
    }
  }

  if message.role == Role::Tool {
    let (tool_call_id, content) = tool_result?;
    return Some(json!({
      "role": "tool",
      "tool_call_id": tool_call_id,
      "content": content,
    }));
  }

  let mut value = json!({ "role": message.role.as_str() });
  value["content"] = if images.is_empty() {
    json!(text)
  } else {
    let mut parts: Vec<Value> = Vec::new();
    if !text.is_empty() {
      parts.push(json!({ "type": "text", "text": text }));
    }
    parts.extend(images);
    Value::Array(parts)
  };
  let has_tool_calls = !tool_calls.is_empty();
  if has_tool_calls {
    value["tool_calls"] = Value::Array(tool_calls);
  }
  if value["content"] == json!("") && !has_tool_calls {
    return None;
  }
  Some(value)
}

fn append(text: &mut String, chunk: &str) {
  if chunk.is_empty() {
    return;
  }
  if !text.is_empty() {
    text.push('\n');
  }
  text.push_str(chunk);
}

/// Whether a model reference is expressible for this endpoint.
///
/// The provider name is metadata for the harness; only the model id travels.
/// This helper exists so callers can say out loud when a reference points at a
/// different provider than the adapter serving it.
pub fn serves(config: &ProviderConfig, model: &ModelRef) -> bool {
  model.provider == config.id || model.provider.is_empty()
}

#[cfg(test)]
mod tests {
  use pi_rs_core::{
    ModelCapabilities, ModelRef, ToolCallBlock, ToolCallId, ToolResultBlock, ToolSpec,
  };

  use super::*;

  fn config() -> ProviderConfig {
    ProviderConfig::local("local", "default-model", "http://127.0.0.1:8080/v1", 32_768)
  }

  fn request(messages: Vec<Message>) -> ModelRequest {
    ModelRequest::new(
      ModelRef::new("local", "qwen3.8-flash"),
      ModelCapabilities::text_only(32_768),
      messages,
    )
  }

  /// A request that lists tools; the capability claim is set by each test, so
  /// the gate itself is what is under test.
  fn with_tools(mut request: ModelRequest) -> ModelRequest {
    request.tools = vec![ToolSpec {
      name: "read".into(),
      description: "Read a file".into(),
      parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
    }];
    request
  }

  #[test]
  fn request_model_wins_and_system_travels_first() {
    let body = request_body(
      &config(),
      &request_with_system(vec![Message::user("hi")], "be terse"),
    );
    assert_eq!(body["model"], "qwen3.8-flash");
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][0]["content"], "be terse");
    assert_eq!(body["messages"][1]["content"], "hi");
  }

  #[test]
  fn config_model_is_the_fallback() {
    let mut req = request(vec![Message::user("hi")]);
    req.model = ModelRef::new("local", "");
    assert_eq!(request_body(&config(), &req)["model"], "default-model");
  }

  #[test]
  fn blank_system_prompt_is_not_sent() {
    let body = request_body(&config(), &request_with_system(vec![], "   "));
    assert_eq!(body["messages"], json!([]));
  }

  fn request_with_system(messages: Vec<Message>, system: &str) -> ModelRequest {
    let mut req = request(messages);
    req.system = Some(system.into());
    req
  }

  #[test]
  fn tools_are_gated_by_the_capability_claim() {
    let unclaimed = with_tools(request(vec![Message::user("x")]));
    assert!(!unclaimed.capabilities.tools);
    let without = request_body(&config(), &unclaimed);
    assert!(
      without.get("tools").is_none() && without.get("tool_choice").is_none(),
      "a tool list cannot be sent to a model that cannot use it: {without}"
    );

    let mut capable = with_tools(request(vec![Message::user("x")]));
    capable.capabilities.tools = true;
    let with = request_body(&config(), &capable);
    assert_eq!(with["tools"][0]["function"]["name"], "read");
    assert_eq!(with["tool_choice"], "auto");
  }

  #[test]
  fn token_limit_field_follows_the_endpoint() {
    let mut req = request(vec![Message::user("x")]);
    req.max_output_tokens = Some(512);
    assert_eq!(request_body(&config(), &req)["max_tokens"], 512);
    assert!(
      request_body(&config(), &req)
        .get("max_completion_tokens")
        .is_none()
    );

    let newer = ProviderConfig {
      max_tokens_field: MaxTokensField::MaxCompletionTokens,
      ..config()
    };
    assert_eq!(
      request_body(&newer, &req)["max_completion_tokens"],
      512,
      "the configured dialect is the one sent"
    );
  }

  #[test]
  fn capability_ceiling_is_used_when_nothing_else_sets_a_limit() {
    let mut req = request(vec![Message::user("x")]);
    req.capabilities.max_output_tokens = Some(1_024);
    assert_eq!(request_body(&config(), &req)["max_tokens"], 1_024);
  }

  #[test]
  fn thinking_dialects_are_distinct() {
    let mut req = request(vec![Message::user("x")]);
    req.thinking = ThinkingLevel::High;

    let effort = request_body(&config(), &req);
    assert_eq!(effort["reasoning_effort"], "high");

    let local = ProviderConfig {
      thinking_input: ThinkingInput::ChatTemplateThinking,
      ..config()
    };
    let template = request_body(&local, &req);
    assert_eq!(template["chat_template_kwargs"]["thinking"], true);
    assert!(template.get("reasoning_effort").is_none());

    let quiet = ProviderConfig {
      thinking_input: ThinkingInput::None,
      ..config()
    };
    let silent = request_body(&quiet, &req);
    assert!(silent.get("reasoning_effort").is_none());
    assert!(silent.get("chat_template_kwargs").is_none());
  }

  #[test]
  fn thinking_off_sends_no_effort_but_still_turns_template_thinking_off() {
    let mut req = request(vec![Message::user("x")]);
    req.thinking = ThinkingLevel::Off;
    assert!(
      request_body(&config(), &req)
        .get("reasoning_effort")
        .is_none()
    );
    let local = ProviderConfig {
      thinking_input: ThinkingInput::ChatTemplateThinking,
      ..config()
    };
    assert_eq!(
      request_body(&local, &req)["chat_template_kwargs"]["thinking"],
      false,
      "some templates keep thinking on unless told otherwise"
    );
  }

  #[test]
  fn xhigh_clamps_upward() {
    let mut req = request(vec![Message::user("x")]);
    req.thinking = ThinkingLevel::Xhigh;
    assert_eq!(request_body(&config(), &req)["reasoning_effort"], "high");
  }

  #[test]
  fn assistant_tool_calls_replay_with_string_arguments() {
    let message = Message::new(
      Role::Assistant,
      vec![
        ContentBlock::text("reading now"),
        ContentBlock::ToolCall(ToolCallBlock {
          id: ToolCallId::from_string("call_1"),
          name: "read".into(),
          arguments: json!({"path": "src/main.rs"}),
        }),
      ],
    );
    let body = request_body(&config(), &request(vec![message]));
    let sent = &body["messages"][0];
    assert_eq!(sent["role"], "assistant");
    assert_eq!(sent["content"], "reading now");
    assert_eq!(sent["tool_calls"][0]["id"], "call_1");
    assert_eq!(
      sent["tool_calls"][0]["function"]["arguments"],
      json!({"path": "src/main.rs"}).to_string(),
      "the wire format wants arguments as a JSON string"
    );
  }

  #[test]
  fn tool_results_address_the_call_they_answer() {
    let message = Message::new(
      Role::Tool,
      vec![ContentBlock::ToolResult(ToolResultBlock {
        id: ToolCallId::from_string("call_1"),
        name: "read".into(),
        state: pi_rs_core::ToolExecutionState::Succeeded,
        text: "fn main() {}".into(),
        is_error: false,
        reduced: false,
      })],
    );
    let body = request_body(&config(), &request(vec![message]));
    assert_eq!(body["messages"][0]["role"], "tool");
    assert_eq!(body["messages"][0]["tool_call_id"], "call_1");
    assert_eq!(body["messages"][0]["content"], "fn main() {}");
  }

  #[test]
  fn reasoning_is_recorded_but_never_replayed() {
    let chunk = pi_rs_core::ReasoningChunk::new(
      "I will read the file",
      pi_rs_core::ReasoningProvenance::Native,
    );
    let message = Message::new(
      Role::Assistant,
      vec![
        ContentBlock::Reasoning(chunk),
        ContentBlock::text("here you go"),
      ],
    );
    let body = request_body(&config(), &request(vec![message]));
    assert_eq!(body["messages"][0]["content"], "here you go");
    assert!(body["messages"][0].get("reasoning_content").is_none());
  }

  #[test]
  fn images_need_a_capability_claim_otherwise_the_omission_is_visible() {
    let message = Message::new(
      Role::User,
      vec![
        ContentBlock::text("what is in this image"),
        ContentBlock::Image {
          mime: "image/png".into(),
          data_base64: "aGk=".into(),
        },
      ],
    );

    let blind = request_body(&config(), &request(vec![message.clone()]));
    let content = blind["messages"][0]["content"].as_str().unwrap();
    assert!(content.contains("what is in this image"));
    assert!(content.contains("image omitted"), "{content}");

    let seeing = ProviderConfig {
      capabilities: ModelCapabilities {
        images: true,
        ..ModelCapabilities::text_only(1)
      },
      ..config()
    };
    let body = request_body(&seeing, &request(vec![message]));
    let parts = body["messages"][0]["content"].as_array().unwrap();
    assert_eq!(parts[0]["type"], "text");
    assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,aGk=");
  }

  #[test]
  fn an_empty_assistant_acknowledgement_is_dropped() {
    let body = request_body(
      &config(),
      &request(vec![Message::new(Role::Assistant, vec![])]),
    );
    assert_eq!(
      body["messages"],
      json!([]),
      "nothing to say, nothing to send"
    );
  }

  #[test]
  fn streaming_is_requested_with_usage_when_enabled() {
    let body = request_body(&config(), &request(vec![Message::user("x")]));
    assert_eq!(body["stream"], true);
    assert_eq!(body["stream_options"]["include_usage"], true);
    let one_shot = ProviderConfig {
      stream: false,
      ..config()
    };
    let body = request_body(&one_shot, &request(vec![Message::user("x")]));
    assert_eq!(body["stream"], false);
    assert!(body.get("stream_options").is_none());
  }

  #[test]
  fn stop_and_temperature_are_optional() {
    let mut req = request(vec![Message::user("x")]);
    let body = request_body(&config(), &req);
    assert!(body.get("temperature").is_none() && body.get("stop").is_none());
    req.temperature = Some(0.2);
    req.stop = vec!["</end>".into()];
    let body = request_body(&config(), &req);
    assert!(
      (body["temperature"].as_f64().unwrap() - 0.2).abs() < 1e-6,
      "{}",
      body["temperature"]
    );
    assert_eq!(body["stop"], json!(["</end>"]));
  }

  #[test]
  fn provider_scope_is_metadata_not_wire_data() {
    assert!(serves(&config(), &ModelRef::new("local", "m")));
    assert!(!serves(&config(), &ModelRef::new("cloud", "m")));
  }
}
