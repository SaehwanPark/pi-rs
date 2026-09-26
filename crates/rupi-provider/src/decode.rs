//! Response decoding and failure normalization.
//!
//! Two responsibilities belong together here because they share the same
//! evidence: what has already been handed to the caller. A mid-stream failure
//! after visible output is a *different* failure than one before any output —
//! the runtime may retry the first and must not silently replay the second. The
//! decoder is therefore the only place that knows, honestly, whether output was
//! emitted.

use std::io;

use rupi_core::{
  CancelToken, CompletionCertainty, FailurePhase, MAX_RESPONSE_EVENTS,
  MAX_RESPONSE_REASONING_BYTES, MAX_RESPONSE_TEXT_BYTES, MAX_RESPONSE_TOOL_CALLS,
  MAX_TOOL_ARGUMENT_BYTES_PER_CALL, MAX_TOOL_ARGUMENT_BYTES_TOTAL, MAX_TOOL_ID_BYTES,
  MAX_TOOL_NAME_BYTES, ProviderEvent, ProviderEventSink, ReasoningExposure, ReasoningProvenance,
  ToolCallBlock, ToolCallId, provider::CompletionUsage,
};
use serde_json::Value;

use crate::config::MAX_ERROR_BODY_BYTES;

/// How a response body ended, which decides whether the turn may be reported
/// as complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamEnd {
  /// The provider sent the `[DONE]` sentinel.
  DoneSentinel,
  /// The body ended with no sentinel: the connection closed.
  EndedWithoutSentinel,
  /// A non-streaming body was read and parsed in full.
  CompleteBody,
}

/// Accumulates chunks and preserves the ordering the harness contract requires.
pub struct Decoder {
  /// What the endpoint declared about its reasoning output, which is what decides
  /// the provenance claim of any thinking text decoded from it.
  exposure: ReasoningExposure,
  /// Original schemas for calls whose optional nulls encode omitted fields
  /// under this endpoint's strict-sampling dialect.
  strict_tool_schemas: std::collections::BTreeMap<String, Value>,
  /// Internal slots are separate from provider indexes and IDs. The provider
  /// may omit either correlation field, and their numeric/string namespaces can
  /// overlap.
  tools: std::collections::BTreeMap<u64, ToolBuilder>,
  tool_indices: std::collections::HashMap<u64, u64>,
  tool_ids: std::collections::HashMap<String, u64>,
  next_tool_slot: u64,
  finish_reason: Option<String>,
  /// Logical prompt tokens reported by the provider before cache accounting.
  logical_prompt_tokens: Option<u64>,
  cache_read_tokens: Option<u64>,
  cache_write_tokens: Option<u64>,
  provider_total_tokens: Option<u64>,
  output_tokens: Option<u64>,
  /// Whether anything the user would call an answer has been emitted.
  emitted_output: bool,
  response_events: usize,
  text_bytes: usize,
  reasoning_bytes: usize,
  tool_argument_bytes: usize,
}

struct ToolBuilder {
  id: Option<String>,
  internal_id: ToolCallId,
  name: String,
  arguments: String,
  correlation_error: Option<String>,
}

impl ToolBuilder {
  fn new() -> Self {
    Self {
      id: None,
      internal_id: ToolCallId::new(),
      name: String::new(),
      arguments: String::new(),
      correlation_error: None,
    }
  }
}

impl Decoder {
  /// A decoder for one endpoint.
  ///
  /// The declared exposure is required rather than defaulted: reasoning text
  /// without a claim about where it came from is the one thing this project may
  /// not record, and a default would silently supply `Native`.
  pub fn new(exposure: ReasoningExposure) -> Self {
    Self {
      exposure,
      strict_tool_schemas: std::collections::BTreeMap::new(),
      tools: std::collections::BTreeMap::new(),
      tool_indices: std::collections::HashMap::new(),
      tool_ids: std::collections::HashMap::new(),
      next_tool_slot: 0,
      finish_reason: None,
      logical_prompt_tokens: None,
      cache_read_tokens: None,
      cache_write_tokens: None,
      provider_total_tokens: None,
      output_tokens: None,
      emitted_output: false,
      response_events: 0,
      text_bytes: 0,
      reasoning_bytes: 0,
      tool_argument_bytes: 0,
    }
  }

  /// Remember the original schemas for tools sent with strict sampling.
  pub(crate) fn with_strict_tool_schemas(
    mut self,
    schemas: std::collections::BTreeMap<String, Value>,
  ) -> Self {
    self.strict_tool_schemas = schemas;
    self
  }

  /// Provenance for thinking text arriving now.
  ///
  /// The claim comes from what the endpoint *declared*, not from the field the
  /// text arrived in. `reasoning_content` holds the model's own thinking on a
  /// llama.cpp or vLLM server and a provider-written summary of hidden reasoning
  /// on a hosted one; deciding by field name alone would record hidden chain of
  /// thought as if it had been exposed. An endpoint that declares nothing leaves
  /// the field name as the only evidence, and the fields read here are the
  /// native-shaped ones.
  fn reasoning_provenance(&self) -> ReasoningProvenance {
    self
      .exposure
      .implied_provenance()
      .unwrap_or(ReasoningProvenance::Native)
  }

  /// Whether visible output has already reached the sink.
  pub fn emitted_output(&self) -> bool {
    self.emitted_output
  }

  /// Consume one streamed chunk, or a full one-shot completion.
  ///
  /// Accepts both shapes because the one-shot body is the same object with
  /// `choices[].message` instead of `choices[].delta`.
  /// The large error variant is deliberate: a failure must carry phase and
  /// partial-output state, and this is a cold path, matching
  /// [`rupi_core::ModelProvider::stream`].
  #[allow(clippy::result_large_err)]
  pub fn chunk(
    &mut self,
    chunk: &Value,
    sink: &mut dyn ProviderEventSink,
  ) -> Result<(), rupi_core::ModelFailure> {
    if let Some(usage) = chunk.get("usage").filter(|u| !u.is_null()) {
      self.logical_prompt_tokens = usage
        .get("prompt_tokens")
        .or_else(|| usage.get("input_tokens"))
        .and_then(Value::as_u64)
        .or(self.logical_prompt_tokens);
      let details = usage
        .get("prompt_tokens_details")
        .or_else(|| usage.get("input_tokens_details"));
      self.cache_read_tokens = details
        .and_then(|details| {
          details
            .get("cached_tokens")
            .and_then(Value::as_u64)
            .or_else(|| details.get("cache_read_tokens").and_then(Value::as_u64))
        })
        .or_else(|| {
          [
            "prompt_cache_hit_tokens",
            "cached_tokens",
            "cache_read_tokens",
          ]
          .iter()
          .find_map(|field| usage.get(*field).and_then(Value::as_u64))
        })
        .or(self.cache_read_tokens);
      self.cache_write_tokens = details
        .and_then(|details| details.get("cache_write_tokens"))
        .or_else(|| usage.get("cache_write_tokens"))
        .and_then(Value::as_u64)
        .or(self.cache_write_tokens);
      self.provider_total_tokens = usage
        .get("total_tokens")
        .and_then(Value::as_u64)
        .or(self.provider_total_tokens);
      self.output_tokens = usage
        .get("completion_tokens")
        .or_else(|| usage.get("output_tokens"))
        .and_then(Value::as_u64)
        .or(self.output_tokens);
    }

    let Some(choices) = chunk.get("choices").and_then(Value::as_array) else {
      // Usage-only chunks, and the empty-choices chunk some servers send last.
      return Ok(());
    };
    for choice in choices {
      let delta = choice
        .get("delta")
        .or_else(|| choice.get("message"))
        .cloned()
        .unwrap_or(Value::Null);
      if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
        self.finish_reason = Some(finish.to_string());
      }
      if let Some(reasoning) = reasoning_text(&delta) {
        if !reasoning.is_empty() {
          self.account_response_event()?;
          if self.reasoning_bytes.saturating_add(reasoning.len()) > MAX_RESPONSE_REASONING_BYTES {
            return Err(self.response_limit_failure(format!(
              "provider response exceeded the {}-byte aggregate reasoning limit",
              MAX_RESPONSE_REASONING_BYTES
            )));
          }
          self.reasoning_bytes += reasoning.len();
          // Server-generated thinking: the endpoint's declaration decides
          // whether that is the model's own reasoning or a summary of it.
          sink.emit(&ProviderEvent::ReasoningDelta {
            text: reasoning.to_string(),
            provenance: self.reasoning_provenance(),
          });
          self.emitted_output = true;
        }
      }
      if let Some(text) = content_text(&delta) {
        if !text.is_empty() {
          self.account_response_event()?;
          if self.text_bytes.saturating_add(text.len()) > MAX_RESPONSE_TEXT_BYTES {
            return Err(self.response_limit_failure(format!(
              "provider response exceeded the {}-byte aggregate text limit",
              MAX_RESPONSE_TEXT_BYTES
            )));
          }
          self.text_bytes += text.len();
          sink.emit(&ProviderEvent::TextDelta(text.to_string()));
          self.emitted_output = true;
        }
      }
      if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
        let uncorrelated_count = calls
          .iter()
          .filter(|call| {
            call.get("index").and_then(Value::as_u64).is_none() && provider_tool_id(call).is_none()
          })
          .count();
        for call in calls {
          self.accumulate_tool(call, uncorrelated_count > 1)?;
        }
      }
    }
    Ok(())
  }

  /// Flush decoded tool calls and report usage.
  ///
  /// Tool calls are emitted only here. A fragment is not a tool call, and the
  /// contract says an adapter may not surface a partially decoded one.
  /// Cold path only: the typed failure carries phase and partial-output state, and
  /// boxing it would add indirection to every match without protecting a hot path.
  #[allow(clippy::result_large_err)]
  pub fn finish(
    mut self,
    end: StreamEnd,
    sink: &mut dyn ProviderEventSink,
  ) -> Result<CompletionUsage, rupi_core::ModelFailure> {
    let tools = std::mem::take(&mut self.tools);
    // A decoded tool call is output the provider produced, even though a user
    // would not call it an answer. Build the complete batch before emitting any
    // call: a duplicate provider id makes result correlation ambiguous, and
    // exposing only the first call would let the runtime execute a partial batch.
    let produced_a_call = !tools.is_empty();
    let mut decoded = Vec::with_capacity(tools.len());
    let mut ids = std::collections::BTreeSet::new();
    for (_, builder) in tools {
      let id = builder
        .id
        .filter(|id| !id.trim().is_empty())
        .map(ToolCallId::from_string)
        .unwrap_or(builder.internal_id);
      if !ids.insert(id.clone()) {
        return Err(decode_failure(format!(
          "duplicate tool call id {} in one provider response",
          id.as_str()
        )));
      }
      let (mut arguments, argument_error) = if builder.arguments.trim().is_empty() {
        (Value::Object(serde_json::Map::new()), None)
      } else {
        match serde_json::from_str::<Value>(&builder.arguments) {
          Ok(arguments) if arguments.is_object() => (arguments, None),
          Ok(_) => (
            Value::Object(serde_json::Map::new()),
            Some("tool arguments must be a JSON object".to_string()),
          ),
          Err(error) => (
            Value::Object(serde_json::Map::new()),
            Some(format!("tool arguments are not valid JSON: {error}")),
          ),
        }
      };
      if argument_error.is_none()
        && let Some(schema) = self.strict_tool_schemas.get(&builder.name)
      {
        restore_omitted_optional_arguments(&mut arguments, schema);
      }
      let reason = match (builder.correlation_error, argument_error) {
        (Some(correlation), Some(arguments)) => Some(format!("{correlation}; {arguments}")),
        (Some(reason), None) | (None, Some(reason)) => Some(reason),
        (None, None) => None,
      };
      let call = ToolCallBlock {
        id: id.clone(),
        name: builder.name,
        arguments,
      };
      if let Some(reason) = reason {
        decoded.push(ProviderEvent::ToolCallRejected {
          id,
          name: call.name,
          reason,
        });
      } else {
        decoded.push(ProviderEvent::ToolCall(call));
      }
    }
    for event in decoded {
      sink.emit(&event);
    }
    // A turn is complete only when the provider said so. A socket that simply
    // closed, or a stream that reached `[DONE]` without ever naming a finish
    // reason or producing output, is an interrupted turn; reporting success
    // there is how a harness silently truncates answers.
    let reported_completion = self.finish_reason.is_some();
    let certainty = match end {
      // The whole body parsed, so the server answered in full.
      StreamEnd::CompleteBody => CompletionCertainty::Certain,
      // `[DONE]` is the provider's own end-of-stream sentinel, so it settles
      // completion for a stream that produced something. An empty stream that
      // merely says `[DONE]` says nothing about completion.
      StreamEnd::DoneSentinel => {
        if reported_completion || self.emitted_output || produced_a_call {
          CompletionCertainty::Certain
        } else {
          return Err(decode_failure(
            "stream ended without a finish reason and without any output".to_string(),
          ));
        }
      }
      // EOF without the sentinel: the connection ended, the provider did not.
      StreamEnd::EndedWithoutSentinel => {
        if reported_completion {
          CompletionCertainty::Certain
        } else if self.emitted_output || produced_a_call {
          // Nothing was done wrong here, and retrying is not this layer's call: a
          // half-answer is already committed content. Report the boundary as
          // uncertain and let the runtime decide what unfinished means.
          CompletionCertainty::Unknown
        } else {
          // Nothing was committed, so a retry cannot duplicate anything. That is an
          // availability failure, which is what the transport layer is for.
          let mut failure = rupi_core::ModelFailure::new(
            rupi_core::ModelFailureKind::Transport,
            rupi_core::FailurePhase::Streaming,
            "stream ended before the provider reported completion",
          );
          failure.partial_output_emitted = false;
          return Err(failure);
        }
      }
    };
    let logical_prompt_tokens = self.logical_prompt_tokens;
    let cache_read_tokens = self.cache_read_tokens.unwrap_or(0);
    let cache_write_tokens = self.cache_write_tokens.unwrap_or(0);
    let uncached_input_tokens = logical_prompt_tokens.map(|logical| {
      logical
        .saturating_sub(cache_read_tokens)
        .saturating_sub(cache_write_tokens)
    });
    let provider_total_tokens = self.provider_total_tokens.or_else(|| {
      logical_prompt_tokens
        .zip(self.output_tokens)
        .map(|(logical, output)| logical.saturating_add(output))
    });
    Ok(CompletionUsage {
      input_tokens: logical_prompt_tokens,
      uncached_input_tokens,
      logical_prompt_tokens,
      cache_read_tokens: self.cache_read_tokens,
      cache_write_tokens: self.cache_write_tokens,
      output_tokens: self.output_tokens,
      provider_total_tokens,
      finish_reason: self.finish_reason,
      certainty,
    })
  }

  #[allow(clippy::result_large_err)]
  fn account_response_event(&mut self) -> Result<(), rupi_core::ModelFailure> {
    if self.response_events >= MAX_RESPONSE_EVENTS {
      return Err(self.response_limit_failure(format!(
        "provider response exceeded the {MAX_RESPONSE_EVENTS}-event aggregate limit"
      )));
    }
    self.response_events += 1;
    Ok(())
  }

  fn response_limit_failure(&self, message: String) -> rupi_core::ModelFailure {
    decode_failure(message).with_partial_output(self.emitted_output)
  }

  /// Cold path only: the typed failure carries phase and partial-output state, and
  /// boxing it would add indirection to every match without protecting a hot path.
  #[allow(clippy::result_large_err)]
  fn accumulate_tool(
    &mut self,
    call: &Value,
    uncorrelated_batch: bool,
  ) -> Result<(), rupi_core::ModelFailure> {
    self.account_response_event()?;
    let provider_index = call.get("index").and_then(Value::as_u64);
    let raw_provider_id = provider_tool_id(call);
    let provider_id = raw_provider_id.filter(|id| id.len() <= MAX_TOOL_ID_BYTES);
    let oversized_id = raw_provider_id.is_some_and(|id| id.len() > MAX_TOOL_ID_BYTES);
    let index_slot = provider_index.and_then(|index| self.tool_indices.get(&index).copied());
    let id_slot = provider_id.and_then(|id| self.tool_ids.get(id).copied());
    let uncorrelated_slots = self.uncorrelated_tool_slots();
    let singleton_fallback =
      (provider_index.is_none() && provider_id.is_none() && !uncorrelated_batch)
        .then(|| self.unique_keyed_open_slot())
        .flatten();

    let slot = match (index_slot, id_slot) {
      (Some(index_slot), Some(id_slot)) if index_slot != id_slot => {
        let reason = "tool-call index and id point to different fragments".to_string();
        self.mark_correlation_error(index_slot, &reason);
        self.mark_correlation_error(id_slot, &reason);
        index_slot
      }
      (Some(index_slot), _) => index_slot,
      (None, Some(id_slot))
        if provider_index.is_some()
          && self
            .tool_indices
            .iter()
            .any(|(index, slot)| *slot == id_slot && Some(*index) != provider_index) =>
      {
        let slot = self.new_tool_slot()?;
        let reason = "one provider tool-call id was associated with multiple indexes";
        self.mark_correlation_error(id_slot, reason);
        self.mark_correlation_error(slot, reason);
        slot
      }
      (None, Some(id_slot)) => id_slot,
      (None, None) if provider_index.is_some() || provider_id.is_some() => {
        if self.tools.len() == 1 && uncorrelated_slots.len() == 1 {
          let slot = uncorrelated_slots[0];
          self.mark_correlation_error(
            slot,
            "provider index or id arrived after uncorrelated fragments",
          );
          slot
        } else {
          let existing_slots = uncorrelated_slots;
          let slot = self.new_tool_slot()?;
          if !existing_slots.is_empty() {
            let reason = "tool-call fragments could not be safely correlated by index or id";
            for existing in existing_slots {
              self.mark_correlation_error(existing, reason);
            }
            self.mark_correlation_error(slot, reason);
          }
          slot
        }
      }
      (None, None) if uncorrelated_batch => {
        let slot = self.new_tool_slot()?;
        let reason = "multiple tool fragments in one chunk had neither an index nor an id";
        let existing_slots = self.open_keyed_tool_slots();
        for existing in existing_slots {
          self.mark_correlation_error(existing, reason);
        }
        self.mark_correlation_error(slot, reason);
        slot
      }
      (None, None) if singleton_fallback.is_some() => {
        singleton_fallback.expect("guard checked singleton fallback")
      }
      (None, None) => {
        let existing_slots = self.open_keyed_tool_slots();
        let slot = self.new_tool_slot()?;
        if !existing_slots.is_empty() {
          let reason = "tool-call fragments could not be safely correlated by index or id";
          for existing in existing_slots {
            self.mark_correlation_error(existing, reason);
          }
          self.mark_correlation_error(slot, reason);
        }
        slot
      }
    };

    if oversized_id {
      self.mark_correlation_error(slot, "provider tool-call id exceeded the response limit");
    }
    if let Some(index) = provider_index {
      self.tool_indices.entry(index).or_insert(slot);
    }
    if let Some(id) = provider_id {
      if self
        .tools
        .get(&slot)
        .and_then(|builder| builder.id.as_deref())
        .is_some_and(|existing| existing != id)
      {
        self.mark_correlation_error(slot, "one provider index carried multiple tool-call ids");
      }
      self.tool_ids.entry(id.to_string()).or_insert(slot);
      self.tools.get_mut(&slot).expect("allocated tool slot").id = Some(id.to_string());
    }
    if provider_index.is_none()
      && provider_id.is_none()
      && singleton_fallback.is_none()
      && !oversized_id
    {
      self.mark_correlation_error(
        slot,
        "tool-call fragment had neither a provider index nor a provider id",
      );
    }
    if let Some(function) = call.get("function") {
      if let Some(name) = function.get("name").and_then(Value::as_str) {
        let current = self
          .tools
          .get(&slot)
          .expect("allocated tool slot")
          .name
          .len();
        if current.saturating_add(name.len()) > MAX_TOOL_NAME_BYTES {
          self.mark_correlation_error(slot, "tool name exceeded the response limit");
        } else {
          self
            .tools
            .get_mut(&slot)
            .expect("allocated tool slot")
            .name
            .push_str(name);
        }
      }
      if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
        let current = self
          .tools
          .get(&slot)
          .expect("allocated tool slot")
          .arguments
          .len();
        let call_bytes = current.saturating_add(arguments.len());
        let total_bytes = self.tool_argument_bytes.saturating_add(arguments.len());
        if call_bytes > MAX_TOOL_ARGUMENT_BYTES_PER_CALL {
          self.mark_correlation_error(slot, "tool arguments exceeded the per-call byte limit");
        } else if total_bytes > MAX_TOOL_ARGUMENT_BYTES_TOTAL {
          self.mark_correlation_error(slot, "tool arguments exceeded the aggregate byte limit");
        } else {
          self
            .tools
            .get_mut(&slot)
            .expect("allocated tool slot")
            .arguments
            .push_str(arguments);
          self.tool_argument_bytes = total_bytes;
        }
      }
    }
    Ok(())
  }

  #[allow(clippy::result_large_err)]
  fn new_tool_slot(&mut self) -> Result<u64, rupi_core::ModelFailure> {
    if self.tools.len() >= MAX_RESPONSE_TOOL_CALLS {
      return Err(self.response_limit_failure(format!(
        "provider response exceeded the {MAX_RESPONSE_TOOL_CALLS}-tool-call limit"
      )));
    }
    let slot = self.next_tool_slot;
    self.next_tool_slot = self
      .next_tool_slot
      .checked_add(1)
      .ok_or_else(|| decode_failure("tool-call slot count is exhausted".to_string()))?;
    self.tools.insert(slot, ToolBuilder::new());
    Ok(slot)
  }

  fn mark_correlation_error(&mut self, slot: u64, reason: &str) {
    if let Some(builder) = self.tools.get_mut(&slot) {
      match builder.correlation_error.as_mut() {
        Some(existing) if !existing.contains(reason) => {
          existing.push_str("; ");
          existing.push_str(reason);
        }
        Some(_) => {}
        None => builder.correlation_error = Some(reason.to_string()),
      }
    }
  }

  fn uncorrelated_tool_slots(&self) -> Vec<u64> {
    let indexed_slots: std::collections::BTreeSet<_> =
      self.tool_indices.values().copied().collect();
    self
      .tools
      .iter()
      .filter(|(slot, builder)| builder.id.is_none() && !indexed_slots.contains(slot))
      .map(|(slot, _)| *slot)
      .collect()
  }

  /// The compatibility fallback is limited to one explicitly keyed builder
  /// without a prior correlation conflict. An uncorrelated fragment cannot
  /// establish its own identity, and two open calls remain ambiguous.
  fn unique_keyed_open_slot(&self) -> Option<u64> {
    let mut candidates = self.open_keyed_tool_slots().into_iter();
    let only = candidates.next()?;
    candidates.next().is_none().then_some(only)
  }

  fn open_keyed_tool_slots(&self) -> Vec<u64> {
    let indexed_slots: std::collections::BTreeSet<_> =
      self.tool_indices.values().copied().collect();
    self
      .tools
      .iter()
      .filter_map(|(slot, builder)| {
        let has_provider_key = builder.id.is_some() || indexed_slots.contains(slot);
        (has_provider_key && builder.correlation_error.is_none()).then_some(*slot)
      })
      .collect()
  }
}

fn provider_tool_id(call: &Value) -> Option<&str> {
  call
    .get("id")
    .and_then(Value::as_str)
    .filter(|id| !id.trim().is_empty())
}

/// A failure while turning a valid response body into harness events.
///
/// The phase is `Normalizing`, not `Streaming`: the provider answered, and the
/// harness could not use the answer. Callers still have to set
/// `partial_output_emitted`, which only the surrounding loop knows.
fn decode_failure(message: String) -> rupi_core::ModelFailure {
  rupi_core::ModelFailure::new(
    rupi_core::ModelFailureKind::Protocol,
    FailurePhase::Normalizing,
    message,
  )
}

/// Where thinking text arrives, across the dialects seen in the wild.
fn reasoning_text(delta: &Value) -> Option<&str> {
  ["reasoning_content", "reasoning", "reasoning_text"]
    .iter()
    .find_map(|field| delta.get(*field).and_then(Value::as_str))
}

/// Visible assistant text, tolerating the string and part-array shapes.
///
/// Returns owned text: a parts array has to be joined anyway, and copying one
/// small delta is not a cost worth contorting the type for.
fn content_text(delta: &Value) -> Option<String> {
  match delta.get("content")? {
    Value::String(text) => Some(text.clone()),
    Value::Array(parts) => {
      // Parts are joined per chunk; the harness concatenates deltas anyway, so
      // a part split across a chunk boundary still reassembles.
      let mut joined = String::new();
      for part in parts {
        if let Some(text) = part.get("text").and_then(Value::as_str) {
          joined.push_str(text);
        }
      }
      (!joined.is_empty()).then_some(joined)
    }
    _ => None,
  }
}

/// Classify an HTTP error status plus body.
pub fn http_failure(
  status: u16,
  body: &str,
  retry_after: Option<&str>,
  phase: FailurePhase,
) -> rupi_core::ModelFailure {
  let message = error_message(body);
  let kind = rupi_core::ModelFailure::classify_http(status, &message);
  let replay_safety =
    if status == 429 || (500..=599).contains(&status) || phase == FailurePhase::PreRequest {
      rupi_core::RequestReplaySafety::Safe
    } else {
      rupi_core::RequestReplaySafety::AmbiguousPostBoundary
    };
  rupi_core::ModelFailure::new(kind, phase, summarize(&message))
    .with_status(status)
    .with_retry_after_ms(retry_after_ms(retry_after, body).unwrap_or(0))
    .with_replay_safety(replay_safety)
    .with_detail(truncate(body, MAX_ERROR_BODY_BYTES / 4))
}

/// Classify a transport-level failure. A read timeout is a distinct kind
/// because failover budgets and backoff differ between "connection never
/// happened" and "the answer stopped arriving".
pub fn transport_failure(text: &str, phase: FailurePhase) -> rupi_core::ModelFailure {
  let kind = if timed_out(text) {
    rupi_core::ModelFailureKind::Timeout
  } else {
    rupi_core::ModelFailureKind::Transport
  };
  rupi_core::ModelFailure::new(kind, phase, summarize(text))
}

/// Classify a failure while the body was streaming.
pub fn stream_failure(error: &io::Error, emitted_output: bool) -> rupi_core::ModelFailure {
  let text = error.to_string();
  let mut failure = transport_failure(&text, FailurePhase::Streaming);
  if matches!(
    error.kind(),
    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
  ) {
    failure.kind = rupi_core::ModelFailureKind::Timeout;
  }
  failure.with_partial_output(emitted_output)
}

/// The user-visible consequence of a cancel: not an availability failure.
pub fn cancelled(emitted_output: bool) -> rupi_core::ModelFailure {
  rupi_core::ModelFailure::new(
    rupi_core::ModelFailureKind::Cancelled,
    if emitted_output {
      FailurePhase::Streaming
    } else {
      FailurePhase::PreRequest
    },
    "cancelled by request",
  )
  .with_partial_output(emitted_output)
}

/// A cancel check between chunks.
pub fn check_cancel(cancel: &CancelToken, emitted_output: bool) -> Option<rupi_core::ModelFailure> {
  cancel.is_cancelled().then(|| cancelled(emitted_output))
}

/// Extract the provider's own error text from an error body.
fn error_message(body: &str) -> String {
  if let Ok(value) = serde_json::from_str::<Value>(body) {
    for path in [
      &["error", "message"][..],
      &["message"][..],
      &["error", "msg"][..],
      &["detail"][..],
    ] {
      let mut cursor = Some(&value);
      for key in path {
        cursor = cursor.and_then(|node| node.get(*key));
      }
      if let Some(Value::String(message)) = cursor {
        if !message.trim().is_empty() {
          return message.clone();
        }
      }
    }
  }
  let trimmed = body.trim();
  if trimmed.is_empty() {
    "<empty body>".to_string()
  } else {
    trimmed.to_string()
  }
}

/// Seconds or milliseconds, from header or body, normalized to milliseconds.
fn retry_after_ms(header: Option<&str>, body: &str) -> Option<u64> {
  if let Some(value) = header
    .and_then(|value| value.trim().parse::<f64>().ok())
    .filter(|value| *value >= 0.0)
  {
    return Some((value * 1_000.0) as u64);
  }
  let value = serde_json::from_str::<Value>(body).ok().and_then(|value| {
    value
      .pointer("/error/retry_after")
      .or_else(|| value.pointer("/retry_after"))
      .and_then(Value::as_u64)
  })?;
  Some(value * 1_000)
}

fn timed_out(text: &str) -> bool {
  let lower = text.to_lowercase();
  lower.contains("timed out") || lower.contains("timeout")
}

pub(crate) fn summarize(text: &str) -> String {
  // Some local servers frame plain-text errors with rows of dashes. Those
  // rows are not a diagnostic and used to hide the actionable line behind a
  // blank-looking `provider_unavailable` message.
  let first_line = text
    .lines()
    .map(str::trim)
    .find(|line| !line.is_empty() && line.chars().any(|character| character.is_alphanumeric()))
    .unwrap_or("");
  truncate(first_line, 240)
}

fn truncate(text: &str, max_chars: usize) -> String {
  let count = text.chars().count();
  if count <= max_chars {
    return text.to_string();
  }
  let kept: String = text.chars().take(max_chars).collect();
  format!("{kept}\u{2026}({} chars truncated)", count - max_chars)
}

/// Remove null sentinels for optional fields after strict-schema sampling.
///
/// OpenAI-style strict schemas make every property required and represent an
/// omitted optional value as `null`. Restore the original argument object
/// shape before the runtime's canonical schema validation and tool dispatch.
fn restore_omitted_optional_arguments(value: &mut Value, schema: &Value) {
  if let (Some(arguments), Some(properties)) = (
    value.as_object_mut(),
    schema.get("properties").and_then(Value::as_object),
  ) {
    let required: std::collections::HashSet<&str> = schema
      .get("required")
      .and_then(Value::as_array)
      .into_iter()
      .flatten()
      .filter_map(Value::as_str)
      .collect();
    let omitted: Vec<String> = properties
      .keys()
      .filter(|name| {
        !required.contains(name.as_str()) && arguments.get(*name).is_some_and(Value::is_null)
      })
      .cloned()
      .collect();
    for name in omitted {
      arguments.remove(&name);
    }
    for (name, child_schema) in properties {
      if let Some(child) = arguments.get_mut(name) {
        restore_omitted_optional_arguments(child, child_schema);
      }
    }
  } else if let (Some(items), Some(values)) = (schema.get("items"), value.as_array_mut()) {
    for item in values {
      restore_omitted_optional_arguments(item, items);
    }
  }
}

#[cfg(test)]
mod tests {
  use rupi_core::{Collector, ModelFailureKind, ReasoningExposure};

  use super::*;

  fn chunk(data: Value) -> Value {
    json!({"choices": [{"index": 0, "delta": data, "finish_reason": null}]})
  }

  use serde_json::json;

  fn decode(chunks: &[Value]) -> (Collector, CompletionUsage) {
    decode_as(ReasoningExposure::Native, chunks)
  }

  /// Decode under a specific declaration. The declaration, not the field name,
  /// is what decides the provenance claim, so tests say which endpoint they are
  /// pretending to talk to.
  fn decode_as(exposure: ReasoningExposure, chunks: &[Value]) -> (Collector, CompletionUsage) {
    let mut collector = Collector::default();
    let mut decoder = Decoder::new(exposure);
    for chunk in chunks {
      decoder.chunk(chunk, &mut collector).unwrap();
    }
    let usage = decoder
      .finish(StreamEnd::DoneSentinel, &mut collector)
      .unwrap();
    (collector, usage)
  }

  #[test]
  fn reasoning_and_text_keep_their_order_and_provenance() {
    let (collector, _) = decode(&[
      chunk(json!({"reasoning_content": "think "})),
      chunk(json!({"reasoning_content": "hard"})),
      chunk(json!({"content": "answer"})),
    ]);
    let events = collector.events();
    assert_eq!(events.len(), 3);
    assert!(matches!(
      &events[0],
      ProviderEvent::ReasoningDelta { text, provenance: ReasoningProvenance::Native }
        if text == "think "
    ));
    assert!(matches!(&events[1], ProviderEvent::ReasoningDelta { text, .. } if text == "hard"));
    assert!(matches!(&events[2], ProviderEvent::TextDelta(t) if t == "answer"));
  }

  #[test]
  fn dialect_variants_of_thinking_all_arrive_as_reasoning() {
    for field in ["reasoning_content", "reasoning", "reasoning_text"] {
      let (collector, _) = decode(&[chunk(json!({field: "thought"}))]);
      assert!(
        matches!(&collector.events()[0], ProviderEvent::ReasoningDelta { text, .. } if text == "thought"),
        "{field}"
      );
    }
  }

  /// The provenance of thinking text follows the endpoint's declaration.
  ///
  /// A hosted endpoint that exposes only a summary of hidden reasoning sends it in
  /// the same `reasoning_*` fields a local server uses for the model's own
  /// thinking. Labelling that text `Native` would claim that hidden chain of
  /// thought had been recovered, and no later layer could undo the claim.
  #[test]
  fn a_declared_summary_is_not_recorded_as_native_thinking() {
    for exposure in [
      ReasoningExposure::ProviderSummary,
      ReasoningExposure::Declared,
    ] {
      let (collector, _) = decode_as(exposure, &[chunk(json!({"reasoning_content": "because"}))]);
      let ProviderEvent::ReasoningDelta { text, provenance } = &collector.events()[0] else {
        panic!(
          "thinking text must still arrive as reasoning: {:?}",
          collector.events()
        );
      };
      assert_eq!(text, "because");
      assert_eq!(
        *provenance,
        exposure
          .implied_provenance()
          .expect("a declared exposure implies a provenance"),
        "{exposure:?}"
      );
    }
  }

  /// The other direction is equally a claim: an endpoint that declares native
  /// reasoning gets `Native`, and one that declares nothing keeps the field name
  /// as the only evidence available. Local servers -- the common case here --
  /// send real thinking in these fields without ever declaring it.
  #[test]
  fn undeclared_exposure_leaves_the_field_name_as_the_evidence() {
    let (collector, _) = decode_as(
      ReasoningExposure::None,
      &[chunk(json!({"reasoning": "think"}))],
    );
    assert!(
      matches!(
        &collector.events()[0],
        ProviderEvent::ReasoningDelta {
          provenance: ReasoningProvenance::Native,
          ..
        }
      ),
      "{:?}",
      collector.events()
    );
  }

  #[test]
  fn content_parts_are_joined() {
    let (collector, _) = decode(&[chunk(json!({
      "content": [{"type": "text", "text": "one "}, {"type": "text", "text": "two"}]
    }))]);
    assert!(matches!(&collector.events()[0], ProviderEvent::TextDelta(t) if t == "one two"));
  }

  #[test]
  fn tool_call_fragments_become_one_decoded_call_after_the_stream_ends() {
    let mut collector = Collector::default();
    let mut decoder = Decoder::new(ReasoningExposure::None);
    for fragment in [
      chunk(json!({
        "tool_calls": [{"index": 0, "id": "call_7", "function": {"name": "read", "arguments": "{\"pa"}}]
      })),
      chunk(json!({"tool_calls": [{"index": 0, "function": {"arguments": "th\": \"a.rs\"}"}}]})),
    ] {
      decoder.chunk(&fragment, &mut collector).unwrap();
    }
    assert!(
      collector.events().is_empty(),
      "a fragment is not a tool call"
    );
    decoder
      .finish(StreamEnd::DoneSentinel, &mut collector)
      .unwrap();
    let events = collector.events();
    assert_eq!(events.len(), 1);
    match &events[0] {
      ProviderEvent::ToolCall(call) => {
        assert_eq!(call.id.as_str(), "call_7");
        assert_eq!(call.name, "read");
        assert_eq!(call.arguments, json!({"path": "a.rs"}));
      }
      other => panic!("expected a tool call, got {other:?}"),
    }
  }

  #[test]
  fn strict_sampling_null_sentinels_restore_optional_argument_omission() {
    let schema = json!({
      "type": "object",
      "additionalProperties": false,
      "properties": {
        "path": {"type": "string"},
        "offset": {"type": "integer"},
        "options": {
          "type": "object",
          "additionalProperties": false,
          "properties": {"mode": {"type": "string"}}
        }
      },
      "required": ["path"]
    });
    let mut decoder = Decoder::new(ReasoningExposure::None)
      .with_strict_tool_schemas(std::collections::BTreeMap::from([("read".into(), schema)]));
    let mut collector = Collector::default();
    decoder
      .chunk(
        &chunk(json!({
          "tool_calls": [{
            "index": 0,
            "id": "call_1",
            "function": {
              "name": "read",
              "arguments": r#"{"path":"a.rs","offset":null,"options":{"mode":null}}"#
            }
          }]
        })),
        &mut collector,
      )
      .unwrap();
    decoder
      .finish(StreamEnd::DoneSentinel, &mut collector)
      .unwrap();
    let ProviderEvent::ToolCall(call) = &collector.events()[0] else {
      panic!("strictly sampled calls remain valid tool calls")
    };
    assert_eq!(call.arguments, json!({"path": "a.rs", "options": {}}));
  }

  #[test]
  fn several_tool_calls_in_one_response_keep_their_indices_apart() {
    let (collector, _) = decode(&[chunk(json!({
      "tool_calls": [
        {"index": 0, "id": "a", "function": {"name": "read", "arguments": "{}"}},
        {"index": 1, "id": "b", "function": {"name": "exec", "arguments": "{}"}},
      ]
    }))]);
    let names: Vec<&str> = collector
      .events()
      .iter()
      .map(|event| match event {
        ProviderEvent::ToolCall(call) => call.name.as_str(),
        _ => "",
      })
      .collect();
    assert_eq!(names, vec!["read", "exec"]);
  }

  #[test]
  fn duplicate_tool_ids_in_one_response_are_rejected_before_emission() {
    let mut collector = Collector::default();
    let mut decoder = Decoder::new(ReasoningExposure::None);
    decoder
      .chunk(
        &chunk(json!({
          "tool_calls": [
            {"index": 0, "id": "call_1", "function": {"name": "read", "arguments": "{}"}},
            {"index": 1, "id": "call_1", "function": {"name": "grep", "arguments": "{}"}},
          ]
        })),
        &mut collector,
      )
      .unwrap();
    let failure = decoder
      .finish(StreamEnd::DoneSentinel, &mut collector)
      .expect_err("one provider response cannot contain two invocations with one id");
    assert_eq!(failure.kind, ModelFailureKind::Protocol);
    assert!(failure.message.contains("duplicate tool call id call_1"));
    assert!(
      collector.events().is_empty(),
      "the ambiguous batch must not partially escape the decoder"
    );
  }

  #[test]
  fn missing_arguments_decode_to_an_empty_object() {
    let (collector, _) = decode(&[chunk(json!({
      "tool_calls": [{"index": 0, "id": "a", "function": {"name": "tick"}}]
    }))]);
    match &collector.events()[0] {
      ProviderEvent::ToolCall(call) => assert_eq!(call.arguments, json!({})),
      other => panic!("{other:?}"),
    }
  }

  #[test]
  fn malformed_tool_arguments_become_rejected_calls_for_model_recovery() {
    let mut collector = Collector::default();
    let mut decoder = Decoder::new(ReasoningExposure::None);
    decoder
      .chunk(
        &chunk(json!({"tool_calls": [{"index": 0, "id": "a", "function": {"name": "read", "arguments": "{\"path\": "}}]})),
        &mut collector,
      )
      .unwrap();
    decoder
      .finish(StreamEnd::DoneSentinel, &mut collector)
      .unwrap();
    assert!(matches!(
      &collector.events()[0],
      ProviderEvent::ToolCallRejected { name, reason, .. }
        if name == "read" && reason.contains("not valid JSON")
    ));
  }

  #[test]
  fn a_tool_call_without_provider_id_gets_a_stable_internal_id() {
    let mut collector = Collector::default();
    let mut decoder = Decoder::new(ReasoningExposure::None);
    decoder
      .chunk(
        &chunk(
          json!({"tool_calls": [{"index": 3, "function": {"name": "read", "arguments": "{}"}}]}),
        ),
        &mut collector,
      )
      .unwrap();
    decoder
      .finish(StreamEnd::DoneSentinel, &mut collector)
      .unwrap();
    let ProviderEvent::ToolCall(call) = &collector.events()[0] else {
      panic!("a valid single call can be correlated by its index");
    };
    assert!(!call.id.as_str().trim().is_empty());
    assert_eq!(call.name, "read");
    assert_eq!(call.arguments, json!({}));
  }

  #[test]
  fn missing_indices_use_ids_to_keep_streamed_calls_apart() {
    let mut collector = Collector::default();
    let mut decoder = Decoder::new(ReasoningExposure::None);
    for fragment in [
      chunk(json!({
        "tool_calls": [
          {"id": "read-id", "function": {"name": "read", "arguments": "{\"path\":"}},
          {"id": "grep-id", "function": {"name": "grep", "arguments": "{\"query\":"}},
        ]
      })),
      chunk(json!({
        "tool_calls": [
          {"id": "read-id", "function": {"arguments": "\"a.rs\"}"}},
          {"id": "grep-id", "function": {"arguments": "\"TODO\"}"}},
        ]
      })),
    ] {
      decoder.chunk(&fragment, &mut collector).unwrap();
    }
    decoder
      .finish(StreamEnd::DoneSentinel, &mut collector)
      .unwrap();
    let calls: Vec<_> = collector
      .events()
      .iter()
      .filter_map(|event| match event {
        ProviderEvent::ToolCall(call) => Some(call),
        _ => None,
      })
      .collect();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].name, "read");
    assert_eq!(calls[0].arguments, json!({"path": "a.rs"}));
    assert_eq!(calls[1].name, "grep");
    assert_eq!(calls[1].arguments, json!({"query": "TODO"}));
  }

  #[test]
  fn a_call_without_index_or_id_gets_an_internal_id_but_is_rejected() {
    let mut collector = Collector::default();
    let mut decoder = Decoder::new(ReasoningExposure::None);
    for fragment in [
      chunk(json!({
        "tool_calls": [{"function": {"name": "read", "arguments": "{\"path\":"}}]
      })),
      chunk(json!({"tool_calls": [{"function": {"arguments": "\"a.rs\"}"}}]})),
    ] {
      decoder.chunk(&fragment, &mut collector).unwrap();
    }
    decoder
      .finish(StreamEnd::DoneSentinel, &mut collector)
      .unwrap();
    let ProviderEvent::ToolCallRejected { id, reason, .. } = &collector.events()[0] else {
      panic!("a call without index or id must not be executed");
    };
    assert!(!id.as_str().trim().is_empty());
    assert!(reason.contains("neither a provider index nor a provider id"));
  }

  #[test]
  fn missing_indices_and_ids_in_a_multi_call_chunk_are_rejected_as_ambiguous() {
    let mut collector = Collector::default();
    let mut decoder = Decoder::new(ReasoningExposure::None);
    decoder
      .chunk(
        &chunk(json!({
          "tool_calls": [
            {"function": {"name": "read", "arguments": "{}"}},
            {"function": {"name": "grep", "arguments": "{}"}},
          ]
        })),
        &mut collector,
      )
      .unwrap();
    decoder
      .finish(StreamEnd::DoneSentinel, &mut collector)
      .unwrap();
    assert_eq!(collector.events().len(), 2);
    let ids: Vec<_> = collector
      .events()
      .iter()
      .map(|event| match event {
        ProviderEvent::ToolCallRejected { id, reason, .. } => {
          assert!(reason.contains("neither an index nor an id"));
          id.as_str().to_string()
        }
        other => panic!("expected rejected call, got {other:?}"),
      })
      .collect();
    assert_ne!(ids[0], ids[1]);
  }

  #[test]
  fn an_uncorrelated_fragment_joins_the_only_unambiguous_open_call() {
    let mut collector = Collector::default();
    let mut decoder = Decoder::new(ReasoningExposure::None);
    for fragment in [
      chunk(json!({
        "tool_calls": [{
          "index": 0,
          "id": "read-id",
          "function": {"name": "read", "arguments": "{\"path\":\"a.rs\""}
        }]
      })),
      chunk(json!({
        "tool_calls": [{"function": {"arguments": "}"}}]
      })),
    ] {
      decoder.chunk(&fragment, &mut collector).unwrap();
    }
    decoder
      .finish(StreamEnd::DoneSentinel, &mut collector)
      .unwrap();
    assert_eq!(collector.events().len(), 1);
    let ProviderEvent::ToolCall(call) = &collector.events()[0] else {
      panic!("one uncorrelated fragment can extend the sole keyed open call");
    };
    assert_eq!(call.id.as_str(), "read-id");
    assert_eq!(call.name, "read");
    assert_eq!(call.arguments, json!({"path": "a.rs"}));
  }

  #[test]
  fn a_missing_key_is_not_guessed_when_multiple_calls_are_open() {
    let mut collector = Collector::default();
    let mut decoder = Decoder::new(ReasoningExposure::None);
    for fragment in [
      chunk(json!({
        "tool_calls": [
          {"index": 0, "id": "read-id", "function": {"name": "read", "arguments": r#"{"path":"a.rs"#}},
          {"index": 1, "id": "grep-id", "function": {"name": "grep", "arguments": r#"{"query":"TODO"#}}
        ]
      })),
      chunk(json!({
        "tool_calls": [{"function": {"name": "write", "arguments": "{}"}}]
      })),
    ] {
      decoder.chunk(&fragment, &mut collector).unwrap();
    }
    decoder
      .finish(StreamEnd::DoneSentinel, &mut collector)
      .unwrap();
    assert_eq!(collector.events().len(), 3);
    assert!(collector.events().iter().all(|event| matches!(
      event,
      ProviderEvent::ToolCallRejected { reason, .. }
        if reason.contains("could not be safely correlated")
    )));
  }

  #[test]
  fn one_provider_index_cannot_change_call_ids_mid_stream() {
    let mut collector = Collector::default();
    let mut decoder = Decoder::new(ReasoningExposure::None);
    for fragment in [
      chunk(json!({
        "tool_calls": [{"index": 0, "id": "first-id", "function": {"name": "read", "arguments": "{}"}}]
      })),
      chunk(json!({
        "tool_calls": [{"index": 0, "id": "second-id", "function": {}}]
      })),
    ] {
      decoder.chunk(&fragment, &mut collector).unwrap();
    }
    decoder
      .finish(StreamEnd::DoneSentinel, &mut collector)
      .unwrap();
    assert!(matches!(
      &collector.events()[0],
      ProviderEvent::ToolCallRejected { reason, .. }
        if reason.contains("one provider index carried multiple tool-call ids")
    ));
  }

  #[test]
  fn usage_and_finish_reason_survive() {
    let mut collector = Collector::default();
    let mut decoder = Decoder::new(ReasoningExposure::None);
    decoder
      .chunk(
        &json!({"choices": [{"index": 0, "delta": {"content": "hi"}, "finish_reason": "tool_calls"}]}),
        &mut collector,
      )
      .unwrap();
    decoder
      .chunk(
        &json!({"choices": [], "usage": {"prompt_tokens": 11, "completion_tokens": 4}}),
        &mut collector,
      )
      .unwrap();
    let usage = decoder
      .finish(StreamEnd::DoneSentinel, &mut collector)
      .unwrap();
    assert_eq!(usage.input_tokens, Some(11));
    assert_eq!(usage.uncached_input_tokens, Some(11));
    assert_eq!(usage.logical_prompt_tokens, Some(11));
    assert_eq!(usage.output_tokens, Some(4));
    assert_eq!(usage.provider_total_tokens, Some(15));
    assert_eq!(usage.finish_reason.as_deref(), Some("tool_calls"));
  }

  #[test]
  fn cache_details_split_logical_prompt_from_inference_input() {
    let mut collector = Collector::default();
    let mut decoder = Decoder::new(ReasoningExposure::None);
    decoder
      .chunk(
        &json!({
          "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
          "usage": {
            "prompt_tokens": 100,
            "prompt_tokens_details": {
              "cached_tokens": 70,
              "cache_write_tokens": 10
            },
            "completion_tokens": 8,
            "total_tokens": 108
          }
        }),
        &mut collector,
      )
      .unwrap();
    let usage = decoder
      .finish(StreamEnd::DoneSentinel, &mut collector)
      .unwrap();
    assert_eq!(usage.logical_prompt_tokens, Some(100));
    assert_eq!(usage.cache_read_tokens, Some(70));
    assert_eq!(usage.cache_write_tokens, Some(10));
    assert_eq!(usage.uncached_input_tokens, Some(20));
    assert_eq!(usage.input_tokens, Some(100));
    assert_eq!(usage.provider_total_tokens, Some(108));
  }

  #[test]
  fn openai_compatible_top_level_cache_aliases_are_normalized() {
    for (cache_fields, expected_cached, expected_uncached) in [
      (json!({"prompt_cache_hit_tokens": 37}), 37, 63),
      (json!({"cached_tokens": 29}), 29, 71),
      (
        json!({
          "prompt_tokens_details": {"cached_tokens": 70},
          "prompt_cache_hit_tokens": 40,
          "cached_tokens": 30
        }),
        70,
        30,
      ),
    ] {
      let mut usage_fields = cache_fields;
      usage_fields["prompt_tokens"] = json!(100);
      let mut collector = Collector::default();
      let mut decoder = Decoder::new(ReasoningExposure::None);
      decoder
        .chunk(
          &json!({
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": usage_fields,
          }),
          &mut collector,
        )
        .unwrap();
      let usage = decoder
        .finish(StreamEnd::DoneSentinel, &mut collector)
        .unwrap();
      assert_eq!(usage.cache_read_tokens, Some(expected_cached));
      assert_eq!(usage.uncached_input_tokens, Some(expected_uncached));
      assert_eq!(usage.logical_prompt_tokens, Some(100));
    }
  }

  #[test]
  fn output_limit_finish_reason_survives_for_runtime_classification() {
    let mut collector = Collector::default();
    let mut decoder = Decoder::new(ReasoningExposure::None);
    decoder
      .chunk(
        &json!({
          "choices": [{
            "index": 0,
            "delta": {"content": "partial"},
            "finish_reason": "length"
          }],
          "usage": {"prompt_tokens": 11, "completion_tokens": 8192}
        }),
        &mut collector,
      )
      .unwrap();

    let usage = decoder
      .finish(StreamEnd::DoneSentinel, &mut collector)
      .expect("the adapter observed the provider boundary");
    assert_eq!(usage.finish_reason.as_deref(), Some("length"));
    assert!(usage.is_certain());
    assert!(usage.stopped_at_output_limit());
    assert_eq!(collector.text(), "partial");
  }

  #[test]
  fn emitted_output_is_reported_honestly() {
    let mut collector = Collector::default();
    let mut decoder = Decoder::new(ReasoningExposure::Native);
    assert!(!decoder.emitted_output());
    decoder
      .chunk(
        &chunk(json!({"reasoning_content": "think"})),
        &mut collector,
      )
      .unwrap();
    assert!(
      decoder.emitted_output(),
      "thinking is output the caller has seen"
    );
  }

  #[test]
  fn mid_stream_failure_marks_partial_output() {
    let mut collector = Collector::default();
    let mut decoder = Decoder::new(ReasoningExposure::None);
    decoder
      .chunk(&chunk(json!({"content": "half"})), &mut collector)
      .unwrap();
    let failure = stream_failure(
      &io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed"),
      decoder.emitted_output(),
    );
    assert_eq!(failure.kind, ModelFailureKind::Transport);
    assert_eq!(failure.phase, FailurePhase::Streaming);
    assert!(failure.partial_output_emitted);
    assert_eq!(
      failure.replay_safety,
      rupi_core::RequestReplaySafety::CommittedOutput
    );
  }

  #[test]
  fn read_timeout_is_a_timeout_not_a_transport_error() {
    let failure = stream_failure(
      &io::Error::new(io::ErrorKind::TimedOut, "read timed out"),
      true,
    );
    assert_eq!(failure.kind, ModelFailureKind::Timeout);
    assert!(
      transport_failure("dns failure", FailurePhase::WaitingForResponse).kind
        == ModelFailureKind::Transport
    );
    assert_eq!(
      transport_failure("connection timed out", FailurePhase::WaitingForResponse).kind,
      ModelFailureKind::Timeout
    );
  }

  #[test]
  fn http_status_maps_and_body_message_is_surfaced() {
    let failure = http_failure(
      429,
      r#"{"error":{"message":"Too many requests","retry_after":2}}"#,
      None,
      FailurePhase::WaitingForResponse,
    );
    assert_eq!(failure.kind, ModelFailureKind::RateLimited);
    assert_eq!(failure.status, Some(429));
    assert_eq!(failure.retry_after_ms, Some(2_000));
    assert_eq!(failure.message, "Too many requests");
    assert!(failure.safe_to_retry(), "an explicit 429 is replay-safe");

    assert_eq!(
      http_failure(
        401,
        r#"{"error":{"message":"bad key"}}"#,
        None,
        FailurePhase::WaitingForResponse
      )
      .kind,
      ModelFailureKind::Authentication
    );
    let unavailable = http_failure(500, "internal", None, FailurePhase::WaitingForResponse);
    assert_eq!(unavailable.kind, ModelFailureKind::ProviderUnavailable);
    assert!(
      unavailable.safe_to_retry(),
      "an explicit 5xx is replay-safe"
    );
  }

  #[test]
  fn context_overflow_beats_the_status_code() {
    let failure = http_failure(
      400,
      r#"{"error":{"message":"This model's maximum context length is 4096 tokens"}}"#,
      None,
      FailurePhase::WaitingForResponse,
    );
    assert_eq!(failure.kind, ModelFailureKind::ContextOverflow);
  }

  #[test]
  fn retry_after_header_wins_and_fractional_seconds_work() {
    let failure = http_failure(429, "{}", Some("1.5"), FailurePhase::WaitingForResponse);
    assert_eq!(failure.retry_after_ms, Some(1_500));
  }

  #[test]
  fn non_json_error_bodies_still_produce_a_readable_message() {
    let failure = http_failure(
      502,
      "<html><body>Bad Gateway\nmore text</body></html>",
      None,
      FailurePhase::WaitingForResponse,
    );
    assert_eq!(failure.message, "<html><body>Bad Gateway");
    assert!(failure.detail.is_some());
  }

  #[test]
  fn separator_wrapped_error_bodies_keep_the_actionable_line() {
    let failure = http_failure(
      500,
      "------------\nUnexpected reasoning effort minimal.\n------------",
      None,
      FailurePhase::WaitingForResponse,
    );
    assert_eq!(
      failure.message, "Unexpected reasoning effort minimal.",
      "decorative framing must not become the provider diagnostic"
    );
  }

  #[test]
  fn long_bodies_are_truncated_with_the_elision_visible() {
    let body = "x".repeat(100_000);
    let failure = http_failure(500, &body, None, FailurePhase::WaitingForResponse);
    // The one-line summary is bounded so it can be a status line, and the
    // elision is visible instead of looking like the server stopped talking.
    assert!(failure.message.chars().count() < 300, "{}", failure.message);
    assert!(failure.message.contains("chars truncated"));
    let detail = failure.detail.unwrap();
    assert!(detail.contains("chars truncated"), "{}", &detail[..80]);
  }

  struct NullEventSink;

  impl rupi_core::ProviderEventSink for NullEventSink {
    fn emit(&mut self, _event: &ProviderEvent) {}
  }

  #[test]
  fn aggregate_limits_apply_across_thousands_of_tiny_sse_events() {
    let frame = "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n";
    let mut body = String::with_capacity(frame.len() * 2_000);
    for _ in 0..2_000 {
      body.push_str(frame);
    }
    let mut stream = crate::sse::SseStream::new(body.as_bytes());
    let mut decoder = Decoder::new(ReasoningExposure::None);
    let mut sink = NullEventSink;
    let mut events = 0;
    while let Some(event) = stream.next_event().unwrap() {
      let value: Value = serde_json::from_str(&event.data).unwrap();
      decoder.chunk(&value, &mut sink).unwrap();
      events += 1;
    }

    assert_eq!(events, 2_000);
    assert_eq!(decoder.text_bytes, 2_000);
    assert_eq!(decoder.response_events, 2_000);
  }

  #[test]
  fn aggregate_text_reasoning_and_event_limits_apply_across_fragments() {
    let mut sink = NullEventSink;
    let mut text = Decoder::new(ReasoningExposure::Native);
    for _ in 0..(MAX_RESPONSE_TEXT_BYTES / 1_024) {
      text
        .chunk(&chunk(json!({"content": "x".repeat(1_024)})), &mut sink)
        .unwrap();
    }
    let error = text
      .chunk(&chunk(json!({"content": "x"})), &mut sink)
      .unwrap_err();
    assert!(error.message.contains("aggregate text limit"));
    assert!(error.partial_output_emitted);

    let mut reasoning = Decoder::new(ReasoningExposure::Native);
    for _ in 0..(MAX_RESPONSE_REASONING_BYTES / 1_024) {
      reasoning
        .chunk(
          &chunk(json!({"reasoning_content": "r".repeat(1_024)})),
          &mut sink,
        )
        .unwrap();
    }
    let error = reasoning
      .chunk(&chunk(json!({"reasoning_content": "r"})), &mut sink)
      .unwrap_err();
    assert!(error.message.contains("aggregate reasoning limit"));
    assert!(error.partial_output_emitted);

    let mut fragmented = Decoder::new(ReasoningExposure::None);
    for _ in 0..MAX_RESPONSE_EVENTS {
      fragmented
        .chunk(&chunk(json!({"content": "x"})), &mut sink)
        .unwrap();
    }
    let error = fragmented
      .chunk(&chunk(json!({"content": "x"})), &mut sink)
      .unwrap_err();
    assert!(error.message.contains("event aggregate limit"));
    assert!(error.partial_output_emitted);
  }

  #[test]
  fn fragmented_tool_arguments_are_bounded_per_call_and_in_aggregate() {
    let mut collector = Collector::default();
    let mut decoder = Decoder::new(ReasoningExposure::None);
    for _ in 0..=(MAX_TOOL_ARGUMENT_BYTES_PER_CALL / 1_024) {
      decoder
        .chunk(
          &chunk(json!({"tool_calls": [{
            "index": 0,
            "id": "large-arguments",
            "function": {"name": "read", "arguments": " ".repeat(1_024)}
          }]})),
          &mut collector,
        )
        .unwrap();
    }
    decoder
      .finish(StreamEnd::DoneSentinel, &mut collector)
      .unwrap();
    assert!(matches!(
      &collector.events()[0],
      ProviderEvent::ToolCallRejected { reason, .. }
        if reason.contains("per-call byte limit")
    ));

    let mut collector = Collector::default();
    let mut decoder = Decoder::new(ReasoningExposure::None);
    for index in 0..10 {
      decoder
        .chunk(
          &chunk(json!({"tool_calls": [{
            "index": index,
            "id": format!("call-{index}"),
            "function": {"name": "read", "arguments": " ".repeat(900_000)}
          }]})),
          &mut collector,
        )
        .unwrap();
    }
    decoder
      .finish(StreamEnd::DoneSentinel, &mut collector)
      .unwrap();
    assert_eq!(collector.events().len(), 10);
    assert!(matches!(
      collector.events().last(),
      Some(ProviderEvent::ToolCallRejected { reason, .. })
        if reason.contains("aggregate byte limit")
    ));
  }

  #[test]
  fn two_thousand_tool_calls_are_bounded_before_emitting_a_partial_batch() {
    let mut collector = Collector::default();
    let mut decoder = Decoder::new(ReasoningExposure::None);
    let calls = (0..2_000)
      .map(|index| {
        json!({
          "index": index,
          "id": format!("call-{index}"),
          "function": {"name": "read", "arguments": "{}"}
        })
      })
      .collect::<Vec<_>>();
    let event = chunk(json!({"tool_calls": calls}));
    let error = decoder.chunk(&event, &mut collector).unwrap_err();

    assert!(error.message.contains("tool-call limit"));
    assert_eq!(decoder.tools.len(), MAX_RESPONSE_TOOL_CALLS);
    assert!(collector.events().is_empty());
  }

  #[test]
  fn cancel_is_not_an_availability_failure() {
    let cancel = CancelToken::new();
    assert!(check_cancel(&cancel, false).is_none());
    cancel.cancel();
    let failure = check_cancel(&cancel, true).unwrap();
    assert_eq!(failure.kind, ModelFailureKind::Cancelled);
    assert!(failure.partial_output_emitted);
    assert!(!failure.kind.is_retryable());
  }
}
