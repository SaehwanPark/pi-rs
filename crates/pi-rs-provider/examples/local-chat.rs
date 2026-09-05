//! Talk to a real local OpenAI-compatible endpoint and print what happened.
//!
//! This exists so the adapter's behavior can be checked against an actual
//! server rather than only against fixtures. It prints typed events with their
//! provenance, which is the claim that is easiest to get wrong.

use std::io::Write;

use pi_rs_core::{
  CancelToken, Collector, Message, ModelCapabilities, ModelProvider, ModelRef, ModelRequest,
  ProviderEvent, ReasoningProvenance, ThinkingLevel,
};
use pi_rs_provider::{OpenAiCompat, ProviderConfig};

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let base_url = std::env::args()
    .nth(1)
    .unwrap_or_else(|| "http://127.0.0.1:8080/v1".to_string());
  let model_id = std::env::args()
    .nth(2)
    .unwrap_or_else(|| "qwen3.8-flash".to_string());
  let prompt = std::env::args()
    .nth(3)
    .unwrap_or_else(|| "Reply with exactly one short sentence.".to_string());
  let context_window: u64 = std::env::var("PI_RS_CONTEXT_WINDOW")
    .ok()
    .and_then(|value| value.parse().ok())
    .unwrap_or(131_072);

  let config = ProviderConfig::local("local", &model_id, &base_url, context_window);
  let provider = OpenAiCompat::new(config)?;
  let model = ModelRef::new("local", &model_id);

  let mut request = ModelRequest::new(
    model.clone(),
    ModelCapabilities {
      context_window,
      ..ModelCapabilities::text_only(1)
    },
    vec![Message::user(&prompt)],
  );
  request.thinking = ThinkingLevel::Medium;
  request.max_output_tokens = Some(128);

  println!("POST {}", provider.config().chat_completions_url());
  let mut collector = Collector::default();
  let result = provider.stream(&request, &mut collector, &CancelToken::new());

  let mut reasoning = String::new();
  let mut answer = String::new();
  let mut provenance = None;
  for event in collector.events() {
    match event {
      ProviderEvent::ReasoningDelta {
        text,
        provenance: source,
      } => {
        reasoning.push_str(text);
        provenance = Some(*source);
      }
      ProviderEvent::TextDelta(text) => answer.push_str(text),
      ProviderEvent::ToolCall(call) => println!("tool_call {} {call:?}", call.name),
    }
  }
  if !reasoning.is_empty() {
    println!(
      "\nreasoning [{} chars, provenance={}]:\n{}",
      reasoning.chars().count(),
      provenance
        .map(ReasoningProvenance::label)
        .unwrap_or("unknown")
        .to_lowercase(),
      reasoning.trim_end()
    );
  }
  println!(
    "\nanswer [{} chars]:\n{}",
    answer.chars().count(),
    answer.trim_end()
  );

  match result {
    Ok(usage) => {
      println!(
        "\ncompleted: finish={:?} in={:?} out={:?}",
        usage.finish_reason, usage.input_tokens, usage.output_tokens
      );
      Ok(())
    }
    Err(failure) => {
      // A failure must be legible, not swallowed: kind, phase, and whether the
      // caller already saw output are the three facts that drive recovery.
      let _ = std::io::stdout().flush();
      Err(
        format!(
          "{} in {:?} (partial_output={}, retryable={})",
          failure.message,
          failure.phase,
          failure.partial_output_emitted,
          failure.safe_to_retry()
        )
        .into(),
      )
    }
  }
}
