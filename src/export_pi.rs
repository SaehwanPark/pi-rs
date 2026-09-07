//! `pi-rs export`: write one session's canonical trace back out in Pi's shape.
//!
//! The shape is not a guess at a specification: it is the set of fields `pi-rs import-pi`
//! reads, which is what makes `export | import-pi` a round trip that can be tested instead
//! of a parity claim that has to be argued. Anything Pi's shape cannot hold is named on
//! stderr as dropped, so an export is never a quieter story than the trace it came from.
//!
//! The command is read-only with respect to the store. It uses [`Store::new`] rather than
//! [`Store::open`] for the same reason `pi-rs trace` does: asking for a file must not be able
//! to mint a state root. What it reads has already passed through the redaction policy on the
//! way into the trace, so an export is redacted output and never the bytes a provider sent.

use std::{
  collections::BTreeMap,
  fs,
  io::{self, Write},
  path::Path,
};

use pi_rs_core::{AgentEvent, ModelRef, RuntimeConfig, SessionId, TraceEntry};
use pi_rs_store::{Store, TraceJournal, WritePolicy};
use serde_json::{Map, Value, json};

use crate::cli::ExportArgs;
use crate::trace;

/// The Pi session-file version this export writes. The importer reads 1, 2 and 3; a new file
/// is written in the newest shape it accepts.
const PI_VERSION: u32 = 3;
const DAY_MS: u64 = 86_400_000;

pub fn execute(args: ExportArgs) -> Result<(), String> {
  let document = export(&args)?;
  // Every loss is named before the bytes are written, so a reader piping this into a file
  // sees on the terminal what the file will not contain.
  for dropped in &document.dropped {
    eprintln!("dropped: {dropped}");
  }
  write_output(args.out.as_deref(), document.text.as_bytes())
}

/// One rendered session: the JSONL, and what the trace held that the shape could not carry.
struct Exported {
  text: String,
  dropped: Vec<String>,
}

/// The assistant reply being collected between a request and its completion.
struct Reply {
  /// The event that introduced this reply, reused as the entry id: ids come from the trace
  /// rather than being minted here.
  id: String,
  timestamp_ms: u64,
  text: String,
  model: Option<ModelRef>,
}

/// Resolve the session, read its trace, and render it.
///
/// All reading happens here, before the caller writes anything, so a session this command
/// cannot write leaves no partial file behind.
fn export(args: &ExportArgs) -> Result<Exported, String> {
  let config_text = fs::read_to_string(&args.config)
    .map_err(|error| format!("cannot read config '{}': {error}", args.config.display()))?;
  let config =
    RuntimeConfig::parse(&config_text).map_err(|error| format!("invalid config: {error}"))?;
  let store = Store::new(
    &config.state_dir,
    WritePolicy::from_retention(&config.trace, &config.redaction),
  );
  let session = trace::resolve_session(&store, args.session.as_deref())?;
  let path = store.layout().trace_path(&session);
  if !path.exists() {
    return Err(format!(
      "session {} has no trace at {}",
      session.as_str(),
      path.display()
    ));
  }
  let report = TraceJournal::read(&path)
    .map_err(|error| format!("cannot read trace '{}': {error}", path.display()))?;
  let mut document = render(&session, &report.items);
  if report.malformed > 0 {
    document.dropped.push(format!(
      "{} trace line(s) this build cannot read (first at line {}); they are not in the export",
      report.malformed,
      report.first_malformed_line.unwrap_or(0)
    ));
  }
  Ok(document)
}

/// Fold the canonical event stream into Pi entries.
///
/// Pi's shape carries one user entry per turn and one assistant entry per reply, while the
/// trace records each reply as streamed deltas. Folding is therefore the whole transform, and
/// what cannot be folded into a message is counted instead of quietly discarded.
fn render(session: &SessionId, items: &[TraceEntry]) -> Exported {
  let mut entries: Vec<Value> = Vec::new();
  // Counts, not a list: a long session holds thousands of deltas, and a reader needs one line
  // per kind of loss, not one per event.
  let mut kinds: BTreeMap<&'static str, u32> = BTreeMap::new();
  let mut reasoning: BTreeMap<&'static str, (u32, usize)> = BTreeMap::new();
  let mut attachments = 0u32;
  let mut cwd = String::new();
  let mut first_ms: Option<u64> = None;
  let mut model: Option<ModelRef> = None;
  let mut reply: Option<Reply> = None;
  let mut previous: Option<String> = None;

  for item in items {
    let id = item.envelope.meta.event_id.as_str();
    let timestamp_ms = item.envelope.meta.timestamp_ms;
    first_ms = Some(first_ms.map_or(timestamp_ms, |seen| seen.min(timestamp_ms)));
    match &item.envelope.event {
      AgentEvent::SessionStarted(started) => cwd = started.working_dir.clone(),
      AgentEvent::UserMessage(message) => {
        attachments += message.attachments;
        // A reply that never completed is still what a model said, so it is written ahead of
        // the turn that followed it rather than dropped on the floor.
        if let Some(unfinished) = reply.take() {
          push_entry(&mut entries, &mut previous, reply_entry(unfinished));
        }
        let mut entry = entry("message", id, timestamp_ms);
        // Pi writes a plain user turn as a string; the block form is for the attachments this
        // export does not carry.
        entry["message"] = json!({ "role": "user", "content": message.text });
        push_entry(&mut entries, &mut previous, entry);
      }
      AgentEvent::ModelRequestStarted(started) => model = Some(started.model.clone()),
      AgentEvent::AssistantDelta(delta) => match reply {
        Some(ref mut existing) => existing.text.push_str(&delta.text),
        None => {
          reply = Some(Reply {
            id: id.to_string(),
            timestamp_ms,
            text: delta.text.clone(),
            model: model.clone().or_else(|| item.envelope.meta.model.clone()),
          });
        }
      },
      AgentEvent::ModelRequestCompleted(completed) => {
        // Later requests reuse the model that served the last one, which is how Pi reads a
        // file that names a model only where it changed.
        model = Some(completed.model.clone());
        if let Some(mut finished) = reply.take() {
          if finished.model.is_none() {
            finished.model = Some(completed.model.clone());
          }
          // Pi dates an assistant entry when the reply landed, not when its first token did.
          finished.timestamp_ms = timestamp_ms;
          push_entry(&mut entries, &mut previous, reply_entry(finished));
        }
      }
      AgentEvent::ReasoningDelta(delta) => {
        let seen = reasoning.entry(delta.provenance.as_str()).or_insert((0, 0));
        seen.0 += 1;
        seen.1 += delta.text.chars().count();
      }
      other => *kinds.entry(kind_name(other)).or_insert(0) += 1,
    }
  }
  if let Some(unfinished) = reply.take() {
    push_entry(&mut entries, &mut previous, reply_entry(unfinished));
  }

  let mut dropped = Vec::new();
  for (provenance, (chunks, characters)) in &reasoning {
    dropped.push(format!(
      "reasoning ({provenance}): {characters} characters in {chunks} chunk(s); Pi's shape has \
       thinking blocks but no provenance field, and relabelling native reasoning as a provider \
       summary is not this tool's call"
    ));
  }
  if attachments > 0 {
    dropped.push(format!(
      "{attachments} attachment(s) on user entries: an export writes the text of a turn, not \
       the bytes beside it"
    ));
  }
  for (kind, count) in &kinds {
    dropped.push(format!(
      "{count} {kind} event(s): this export writes user and assistant message entries, so tool, \
       context, and lifecycle detail stays in the trace"
    ));
  }

  let mut lines = vec![header(session, &cwd, first_ms).to_string()];
  lines.extend(entries.iter().map(Value::to_string));
  Exported {
    text: format!("{}\n", lines.join("\n")),
    dropped,
  }
}

/// Append one entry, linking it to the entry before it.
///
/// The conversation this export writes is linear by construction: it holds what was said, in
/// the order it was said, and Pi reads `parentId` as that order.
fn push_entry(entries: &mut Vec<Value>, previous: &mut Option<String>, mut entry: Value) {
  if let Some(parent) = previous {
    entry["parentId"] = json!(parent);
  }
  *previous = entry["id"].as_str().map(str::to_string);
  entries.push(entry);
}

/// The fields every Pi entry shares. `parentId` starts null because the writer fills each one
/// in from the entry it emitted just before.
fn entry(kind: &str, id: &str, timestamp_ms: u64) -> Value {
  let mut object = Map::new();
  object.insert("type".to_string(), json!(kind));
  object.insert("id".to_string(), json!(id));
  object.insert("parentId".to_string(), Value::Null);
  object.insert("timestamp".to_string(), json!(pi_timestamp(timestamp_ms)));
  Value::Object(object)
}

/// The header line: metadata, not part of the entry tree, so it carries no `parentId`.
fn header(session: &SessionId, cwd: &str, timestamp_ms: Option<u64>) -> Value {
  let mut object = Map::new();
  object.insert("type".to_string(), json!("session"));
  object.insert("version".to_string(), json!(PI_VERSION));
  object.insert("id".to_string(), json!(session.as_str()));
  if let Some(timestamp_ms) = timestamp_ms {
    object.insert("timestamp".to_string(), json!(pi_timestamp(timestamp_ms)));
  }
  if !cwd.is_empty() {
    object.insert("cwd".to_string(), json!(cwd));
  }
  Value::Object(object)
}

/// One assistant entry: prose in a `text` block, and the model that produced it when the trace
/// knew which model that was.
fn reply_entry(reply: Reply) -> Value {
  let mut message = json!({
    "role": "assistant",
    "content": [ { "type": "text", "text": reply.text } ],
  });
  if let Some(model) = reply.model {
    message["provider"] = json!(model.provider);
    message["model"] = json!(model.model);
  }
  let mut entry = entry("message", &reply.id, reply.timestamp_ms);
  entry["message"] = message;
  entry
}

/// The event kinds, by the name the trace uses for them.
///
/// Exhaustive on purpose: a new event kind must be considered by this export rather than fall
/// into the bucket that says it was dropped.
fn kind_name(event: &AgentEvent) -> &'static str {
  match event {
    AgentEvent::SessionStarted(_) => "session_started",
    AgentEvent::UserMessage(_) => "user_message",
    AgentEvent::ModelRequestStarted(_) => "model_request_started",
    AgentEvent::ReasoningDelta(_) => "reasoning_delta",
    AgentEvent::AssistantDelta(_) => "assistant_delta",
    AgentEvent::ModelRequestCompleted(_) => "model_request_completed",
    AgentEvent::ModelRetry(_) => "model_retry",
    AgentEvent::ModelFailover(_) => "model_failover",
    AgentEvent::ModelEpochStarted(_) => "model_epoch_started",
    AgentEvent::ToolRequested(_) => "tool_requested",
    AgentEvent::ToolStarted(_) => "tool_started",
    AgentEvent::ToolCompleted(_) => "tool_completed",
    AgentEvent::ToolFailed(_) => "tool_failed",
    AgentEvent::ToolUnknown(_) => "tool_unknown",
    AgentEvent::ExternalContextRetrieved(_) => "external_context_retrieved",
    AgentEvent::ContextReduced(_) => "context_reduced",
    AgentEvent::ContextCompactionStarted(_) => "context_compaction_started",
    AgentEvent::ContextCompactionCompleted(_) => "context_compaction_completed",
    AgentEvent::CheckpointCreated(_) => "checkpoint_created",
    AgentEvent::TurnCompleted(_) => "turn_completed",
    AgentEvent::Diagnostic(_) => "diagnostic",
    AgentEvent::SessionEnded(_) => "session_ended",
  }
}

/// Epoch milliseconds as the `Z`-suffix stamp Pi writes.
///
/// The importer parses this shape back into milliseconds; keeping the two directions separate,
/// and tested against each other, is what stops an export from shifting a session's clock.
fn pi_timestamp(milliseconds: u64) -> String {
  let days = (milliseconds / DAY_MS) as i64;
  let time_of_day = milliseconds % DAY_MS;
  // Days since 0000-03-01, which makes the leap day the last day of the year and so removes it
  // from the month arithmetic below.
  let shifted = days + 719_468;
  let era = shifted / 146_097;
  let day_of_era = (shifted - era * 146_097) as u64;
  let year_of_era =
    (day_of_era - day_of_era / 1_461 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
  let year = year_of_era as i64 + era * 400;
  let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
  let month_probe = (5 * day_of_year + 2) / 153;
  let day = day_of_year - (153 * month_probe + 2) / 5 + 1;
  let month = if month_probe < 10 {
    month_probe + 3
  } else {
    month_probe - 9
  };
  let year = if month <= 2 { year + 1 } else { year };
  format!(
    "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
    time_of_day / 3_600_000,
    (time_of_day / 60_000) % 60,
    (time_of_day / 1_000) % 60,
    time_of_day % 1_000,
  )
}

/// Send the JSONL where the caller pointed: stdout, or a path the caller named.
fn write_output(out: Option<&Path>, bytes: &[u8]) -> Result<(), String> {
  let Some(out) = out else {
    let mut stdout = io::stdout();
    return match stdout.write_all(bytes) {
      // A closed pipe is how `pi-rs export s | head` ends. It is not an export failure.
      Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
      Err(error) => Err(format!("cannot write stdout: {error}")),
      Ok(()) => stdout.flush().map_err(|error| format!("cannot write stdout: {error}")),
    };
  };
  match fs::symlink_metadata(out) {
    // A link at the destination points somewhere the caller never typed; following it would
    // let an export write outside the path that was named.
    Ok(meta) if meta.file_type().is_symlink() => {
      return Err(format!("'{}' is a symlink; write to a plain path", out.display()));
    }
    Ok(meta) if meta.is_dir() => return Err(format!("'{}' is a directory", out.display())),
    Ok(_) => {}
    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
    Err(error) => return Err(format!("cannot inspect '{}': {error}", out.display())),
  }
  // Only the directories this path needs are created, and only for a path the caller gave.
  if let Some(parent) = out.parent().filter(|parent| !parent.as_os_str().is_empty()) {
    fs::create_dir_all(parent).map_err(|error| {
      format!(
        "cannot create '{}': {error}",
        parent.display()
      )
    })?;
  }
  fs::write(out, bytes).map_err(|error| format!("cannot write '{}': {error}", out.display()))
}

#[cfg(test)]
mod tests {
  use pi_rs_core::{
    AgentEvent, AssistantDelta, EventEnvelope, EventMeta, ModelCapabilities, ModelRef,
    ModelRequestCompleted, ModelRequestStarted, ReasoningDelta, ReasoningExposure,
    ReasoningProvenance, SessionId, SessionStarted, TraceEntry, TraceId, UserMessage,
  };
  use pi_rs_store::pi_import;

  use super::{Exported, pi_timestamp, render};

  fn model() -> ModelRef {
    ModelRef::new("anthropic", "claude-opus-4-8")
  }

  fn event(event: AgentEvent, timestamp_ms: u64) -> TraceEntry {
    let mut meta = EventMeta::new(SessionId::from_string("session-under-export"), TraceId::new());
    meta.timestamp_ms = timestamp_ms;
    TraceEntry {
      envelope: EventEnvelope::new(meta, event),
      redactions: 0,
      raw_payload: false,
      raw_ref: None,
    }
  }

  /// A turn: the user entry, then a reply that arrived as two deltas and a completion.
  fn turn(question: &str, first: &str, second: &str, from_ms: u64) -> Vec<TraceEntry> {
    vec![
      event(
        AgentEvent::UserMessage(UserMessage {
          text: question.to_string(),
          attachments: 0,
        }),
        from_ms,
      ),
      event(
        AgentEvent::ModelRequestStarted(ModelRequestStarted {
          epoch: 0,
          model: model(),
          message_count: 1,
          context_tokens_est: 0,
          tools_exposed: 0,
        }),
        from_ms + 1,
      ),
      event(
        AgentEvent::AssistantDelta(AssistantDelta {
          text: first.to_string(),
          chunk_index: 0,
        }),
        from_ms + 2,
      ),
      event(
        AgentEvent::AssistantDelta(AssistantDelta {
          text: second.to_string(),
          chunk_index: 1,
        }),
        from_ms + 3,
      ),
      event(
        AgentEvent::ModelRequestCompleted(ModelRequestCompleted {
          epoch: 0,
          model: model(),
          finish_reason: None,
          input_tokens: None,
          output_tokens: None,
          duration_ms: 1_000,
          tool_calls: 0,
          reasoning_provenance: None,
        }),
        from_ms + 4,
      ),
    ]
  }

  fn session_started(working_dir: &str) -> TraceEntry {
    event(
      AgentEvent::SessionStarted(SessionStarted {
        working_dir: working_dir.to_string(),
        model: model(),
        capabilities: ModelCapabilities {
          exposed_reasoning: ReasoningExposure::Native,
          ..ModelCapabilities::text_only(1_024)
        },
        resumed: false,
      }),
      1_000,
    )
  }

  #[test]
  fn stamps_are_written_in_the_shape_the_importer_reads() {
    assert_eq!(pi_timestamp(0), "1970-01-01T00:00:00.000Z");
    assert_eq!(pi_timestamp(946_728_000_000), "2000-01-01T12:00:00.000Z");
    // A fraction only, the last millisecond of a day, a whole date, and a value whose whole
    // seconds a naive formatter would drop.
    for milliseconds in [987u64, 86_399_999, 1_753_000_000_000, 1_753_086_399_999] {
      let stamp = pi_timestamp(milliseconds);
      let text = format!(
        concat!(
          "{{\"type\":\"session\",\"version\":3,\"id\":\"s1\"}}\n",
          "{{\"type\":\"message\",\"id\":\"e1\",\"parentId\":null,\"timestamp\":\"{stamp}\",",
          "\"message\":{{\"role\":\"user\",\"content\":\"x\"}}}}\n"
        ),
      );
      let parsed = pi_import::parse("emitted.jsonl", &text).expect("the emitted line parses");
      let entry = parsed.entries.first().expect("the message entry");
      assert_eq!(entry.timestamp_ms(), Some(milliseconds), "{stamp} drifted");
    }
  }

  #[test]
  fn a_streamed_reply_becomes_one_assistant_entry_in_trace_order() {
    let mut items = vec![session_started("/home/dev/app")];
    items.extend(turn("which files changed?", "Checking the ", "working tree.", 2_000));
    items.extend(turn("and this error?", "It compiles.", "", 9_000));
    let session = SessionId::from_string("session-under-export");
    let Exported { text, dropped } = render(&session, &items);

    let lines: Vec<serde_json::Value> = text
      .lines()
      .map(|line| serde_json::from_str(line).expect("each line is one JSON object"))
      .collect();
    assert_eq!(lines.len(), 5, "header plus two turns, one entry each: {text}");
    assert_eq!(lines[0]["type"], "session");
    assert_eq!(lines[0]["cwd"], "/home/dev/app");
    let roles: Vec<&str> = lines[1..]
      .iter()
      .map(|line| line["message"]["role"].as_str().unwrap())
      .collect();
    assert_eq!(roles, ["user", "assistant", "user", "assistant"]);
    assert_eq!(lines[2]["message"]["content"][0]["text"], "Checking the working tree.");
    assert_eq!(lines[2]["message"]["model"], "claude-opus-4-8");
    // Each entry is a child of the one before it: the export is one line of history.
    let ids: Vec<&str> = lines[1..].iter().map(|line| line["id"].as_str().unwrap()).collect();
    let parents: Vec<&str> = lines[2..]
      .iter()
      .map(|line| line["parentId"].as_str().unwrap())
      .collect();
    assert_eq!(parents, [&ids[0], &ids[1], &ids[2]]);
    assert!(dropped.is_empty(), "a plain conversation loses nothing: {dropped:?}");
  }

  #[test]
  fn reasoning_is_reported_by_provenance_and_the_file_is_still_importable() {
    let mut items = vec![session_started("/work")];
    items.extend(turn("why?", "answer", "", 2_000));
    items.insert(
      2,
      event(
        AgentEvent::ReasoningDelta(ReasoningDelta {
          text: "a summary the provider wrote".to_string(),
          provenance: ReasoningProvenance::ProviderSummary,
          chunk_index: 0,
        }),
        2_001,
      ),
    );
    let session = SessionId::from_string("session-under-export");
    let rendered = render(&session, &items);
    let named = rendered
      .dropped
      .iter()
      .find(|line| line.contains("provider_summary"))
      .expect("the dropped reasoning names its provenance");
    assert!(named.contains("28 characters"), "{named}");
    assert!(
      !rendered.text.contains("a summary the provider wrote"),
      "reasoning text must not be relabelled into the file: {}",
      rendered.text
    );
    // The emitted file is one this version of the importer accepts, on the path it claims.
    let parsed = pi_import::parse("emitted.jsonl", &rendered.text).expect("import accepts it");
    assert_eq!(parsed.header.cwd, "/work");
    assert_eq!(parsed.active_path().expect("a linear tree").len(), 2);
  }
}
