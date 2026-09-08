//! Reading a Pi session file.
//!
//! This module turns one Pi JSONL session into the material a pi-rs session is made of.
//! It is a *prototype* in a specific sense: it imports what Pi's format states, and reports
//! what it cannot carry rather than inventing a plausible substitute. That bias is the
//! whole design. Pi records more than pi-rs models -- branch trees, extension entries,
//! labels, thinking levels, per-request costs -- and an importer that quietly mapped those
//! onto the nearest-looking pi-rs event would produce a session that looks native and
//! isn't. The rule used here:
//!
//! - A Pi fact with a pi-rs equivalent becomes that event.
//! - A Pi fact with no pi-rs equivalent becomes a report line naming the entry type and
//!   the reason.
//! - A pi-rs field that Pi does not record gets the conservative value, never a friendly
//!   one, and the report says so once.
//!
//! Two invariants keep the result honest:
//!
//! - **An import never executes anything.** Pi's file is a record of tool calls that
//!   already happened elsewhere. They are imported as records; no call is replayed, and
//!   imported calls are marked as *not* read-only because that is the direction in which a
//!   mistake is safe.
//! - **An import never pretends to be native.** The header records `imported_from`, and the
//!   model that owned epoch 0 is the one Pi's own entries name, or an explicit `unknown`
//!   rather than a guess.
//!
//! Parsing is deliberately untyped over [`serde_json::Value`]. Pi's format has more entry
//! and message roles than any Rust enum here could keep up with, and forward compatibility
//! is worth more than a compile-time guarantee that would be satisfied by ignoring the
//! future. Unknown shapes are reported, not rejected: a session that also contains an
//! entry type this file has never seen still imports.

use std::{collections::BTreeMap, fs, path::Path};

use serde_json::Value;

use pi_rs_core::{
  AgentEvent, AssistantDelta, Diagnostic, DiagnosticLevel, EventEnvelope, EventMeta, ModelRef,
  ModelRequestCompleted, ModelRequestStarted, ReasoningDelta, ReasoningProvenance, SessionHeader,
  SessionId, ToolCallId, ToolCompleted, ToolExecutionState, ToolFailed, ToolRequested, TraceId,
  UserMessage, session::SESSION_SCHEMA_VERSION,
};

use crate::{Store, StoreError};

/// A parsed Pi session file: its header line and every entry that followed it.
#[derive(Debug, Clone)]
pub struct PiSession {
  /// Path the session was read from, recorded because skipped data stays there.
  pub path: String,
  pub header: PiHeader,
  pub entries: Vec<PiEntry>,
  /// Lines that were not a JSON object, with the file's own line numbers. Damaged lines
  /// are reported and skipped; they do not fail an import that was otherwise readable.
  pub damaged_lines: Vec<String>,
}

/// The first line of a Pi file: metadata, not part of the entry tree.
#[derive(Debug, Clone)]
pub struct PiHeader {
  /// 1, 2, or 3. Pi migrates older sessions on load; this reader tolerates all three.
  pub version: u32,
  pub id: String,
  /// The directory Pi was working in. Empty when the file did not record one.
  pub cwd: String,
  /// When the session started, as Pi wrote it. Empty when the header had none.
  pub timestamp: String,
  pub parent_session: Option<String>,
}

/// One entry line, kept as raw JSON plus the fields every entry shares.
#[derive(Debug, Clone)]
pub struct PiEntry {
  pub line: usize,
  pub id: String,
  pub parent: Option<String>,
  pub entry_type: String,
  pub value: Value,
}

/// Why an import could not proceed at all.
///
/// Everything else is a report, not an error: a Pi file that contains nothing importable
/// is still a readable file, and the answer to "what happened to my session?" is a report
/// saying so, not a stack of reasons.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PiImportError {
  /// The file could not be read.
  Unreadable { path: String, reason: String },
  /// The first line was missing or is not a Pi session header.
  MissingHeader { path: String, reason: String },
  /// Two entries claim the same id, so the tree has no defined parent for them.
  DuplicateId { id: String },
  /// An entry points at a parent id that the file does not contain.
  DanglingParent { id: String, parent: String },
  /// Following `parentId` from the leaf came back around.
  Cycle { id: String },
}

impl std::fmt::Display for PiImportError {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Unreadable { path, reason } => {
        write!(formatter, "cannot read Pi session '{path}': {reason}")
      }
      Self::MissingHeader { path, reason } => {
        write!(formatter, "'{path}' is not a Pi session: {reason}")
      }
      Self::DuplicateId { id } => write!(formatter, "two Pi entries share the id '{id}'"),
      Self::DanglingParent { id, parent } => write!(
        formatter,
        "Pi entry '{id}' names a parent '{parent}' that the file does not contain"
      ),
      Self::Cycle { id } => {
        write!(
          formatter,
          "Pi entry '{id}' is its own ancestor through parentId"
        )
      }
    }
  }
}

impl std::error::Error for PiImportError {}

/// Read and parse a Pi session file.
pub fn read(path: &Path) -> Result<PiSession, PiImportError> {
  let display = path.display().to_string();
  let text = fs::read_to_string(path).map_err(|error| PiImportError::Unreadable {
    path: display.clone(),
    reason: error.to_string(),
  })?;
  parse(&display, &text)
}

/// Parse an already-read Pi session file.
///
/// Split from [`read`] so that the whole format is testable without a filesystem.
pub fn parse(path: &str, text: &str) -> Result<PiSession, PiImportError> {
  let mut header: Option<PiHeader> = None;
  let mut entries: Vec<PiEntry> = Vec::new();
  let mut damaged_lines = Vec::new();
  for (index, line) in text.lines().enumerate() {
    let line_number = index + 1;
    if line.trim().is_empty() {
      continue;
    }
    let value: Value = match serde_json::from_str(line) {
      Ok(value @ Value::Object(_)) => value,
      Ok(_) => {
        damaged_lines.push(format!("line {line_number}: not a JSON object, skipped"));
        continue;
      }
      Err(error) => {
        damaged_lines.push(format!("line {line_number}: {error}, skipped"));
        continue;
      }
    };
    let entry_type = string_field(&value, "type").unwrap_or_default().to_string();
    if header.is_none() {
      if entry_type == "session" {
        header = Some(PiHeader {
          version: number_field(&value, "version").unwrap_or(1) as u32,
          id: string_field(&value, "id").unwrap_or_default().to_string(),
          cwd: string_field(&value, "cwd").unwrap_or_default().to_string(),
          timestamp: string_field(&value, "timestamp")
            .unwrap_or_default()
            .to_string(),
          parent_session: string_field(&value, "parentSession").map(String::from),
        });
        continue;
      }
      return Err(PiImportError::MissingHeader {
        path: path.to_string(),
        reason: format!(
          "the first line is type '{entry_type}'; a Pi session starts with type 'session'"
        ),
      });
    }
    entries.push(PiEntry {
      line: line_number,
      // No fallback id: an empty id is how a version 1 session says "this format has no
      // tree", and inventing one would make the file look branched when it cannot be.
      id: string_field(&value, "id").unwrap_or_default().to_string(),
      parent: string_field(&value, "parentId").map(String::from),
      entry_type,
      value,
    });
  }
  let header = header.ok_or_else(|| PiImportError::MissingHeader {
    path: path.to_string(),
    reason: "no session header line".to_string(),
  })?;
  Ok(PiSession {
    path: path.to_string(),
    header,
    entries,
    damaged_lines,
  })
}

impl PiSession {
  /// The active path through the entry tree, oldest first.
  ///
  /// Pi stores a tree, and the position of the cursor is in-memory state that the file
  /// does not record. The convention this follows is Pi's own for a file nobody re-branched
  /// after writing: the newest entry written is the leaf, and the active context is the
  /// walk from it to the root. Everything off that path is a branch that was abandoned,
  /// and the caller reports how many entries it dropped instead of pretending the file was
  /// linear.
  ///
  /// Version 1 sessions carry no ids at all. They are linear by definition, so the walk is
  /// file order.
  pub fn active_path(&self) -> Result<Vec<&PiEntry>, PiImportError> {
    if self.entries.is_empty() {
      return Ok(Vec::new());
    }
    let first = &self.entries[0];
    if first.id.is_empty() || self.entries.iter().any(|entry| entry.id.is_empty()) {
      return Ok(self.entries.iter().collect());
    }
    let leaf = self
      .entries
      .last()
      .expect("entries checked non-empty above");
    let by_id: BTreeMap<&str, &PiEntry> = self
      .entries
      .iter()
      .map(|entry| (entry.id.as_str(), entry))
      .collect();
    if by_id.len() != self.entries.len() {
      let mut seen: BTreeMap<&str, u32> = BTreeMap::new();
      for entry in &self.entries {
        *seen.entry(entry.id.as_str()).or_insert(0) += 1;
      }
      let (duplicate, _) = seen
        .into_iter()
        .find(|(_, count)| *count > 1)
        .expect("a duplicate must exist when ids collapse");
      return Err(PiImportError::DuplicateId {
        id: duplicate.to_string(),
      });
    }
    let mut path: Vec<&PiEntry> = Vec::new();
    let mut visited: BTreeMap<&str, ()> = BTreeMap::new();
    let mut cursor: Option<&PiEntry> = Some(leaf);
    while let Some(entry) = cursor {
      if visited.insert(entry.id.as_str(), ()).is_some() {
        return Err(PiImportError::Cycle {
          id: entry.id.clone(),
        });
      }
      path.push(entry);
      let Some(parent) = entry.parent.as_deref() else {
        break;
      };
      cursor = Some(
        by_id
          .get(parent)
          .copied()
          .ok_or_else(|| PiImportError::DanglingParent {
            id: entry.id.clone(),
            parent: parent.to_string(),
          })?,
      );
    }
    path.reverse();
    Ok(path)
  }

  /// Entries that are not on the active path, i.e. abandoned branches.
  pub fn off_path(&self, active: &[&PiEntry]) -> Vec<&PiEntry> {
    let on: BTreeMap<&str, ()> = active.iter().map(|entry| (entry.id.as_str(), ())).collect();
    self
      .entries
      .iter()
      .filter(|entry| !on.contains_key(entry.id.as_str()))
      .collect()
  }
}

impl PiEntry {
  /// The field every Pi entry shares, read as text.
  pub(crate) fn str(&self, key: &str) -> Option<&str> {
    self.value.get(key).and_then(Value::as_str)
  }

  /// The field every Pi entry shares, read as a count.
  pub(crate) fn num(&self, key: &str) -> Option<u64> {
    self.value.get(key).and_then(Value::as_u64)
  }

  /// The entry's ISO-8601 timestamp as milliseconds since the epoch.
  ///
  /// Pi writes `timestamp` as an ISO string and pi-rs records epoch milliseconds, so the
  /// conversion has to happen somewhere. It happens here, without a date-parsing dependency:
  /// the accepted shape is what Pi writes (`YYYY-MM-DDTHH:MM:SS[.sss][Z|+-HH:MM]`), anything
  /// else is `None`, and the caller decides what a missing time means.
  pub fn timestamp_ms(&self) -> Option<u64> {
    self.str("timestamp").and_then(is_to_millis)
  }
}

/// Convert the ISO-8601 shape Pi writes into epoch milliseconds.
pub(crate) fn is_to_millis(text: &str) -> Option<u64> {
  let (date, time) = text.split_once('T')?;
  let mut date_parts = date.split('-');
  let year: i64 = date_parts.next()?.parse().ok()?;
  let month: u32 = date_parts.next()?.parse().ok()?;
  let day: u32 = date_parts.next()?.parse().ok()?;
  let (clock, offset_minutes) = split_zone(time);
  let mut time_parts = clock.split(':');
  let hour: u64 = time_parts.next()?.parse().ok()?;
  let minute: u64 = time_parts.next()?.parse().ok()?;
  let second: f64 = time_parts.next()?.parse().ok()?;
  let days = days_from_epoch(year, month, day)?;
  // The whole seconds are added here and only the fraction below; dropping the whole part
  // is the kind of bug a "round trip a minute boundary" test would not catch.
  let seconds = days * 86_400 + (hour * 3_600 + minute * 60) as i64 + second as i64;
  let millis =
    seconds * 1_000 + (second.fract() * 1_000.0).round() as i64 - offset_minutes * 60_000;
  u64::try_from(millis).ok()
}

/// Split a time part into the wall clock and its zone offset in minutes.
///
/// A time that states no zone is read as UTC. ISO-8601 calls a zone-less time local, and a
/// consistently wrong reading beats discarding the stamp: entries inside one file keep their
/// relative order either way, and Pi writes `Z` in practice.
fn split_zone(time: &str) -> (&str, i64) {
  let bytes = time.as_bytes();
  let marked = bytes.len() >= 6
    && matches!(bytes[bytes.len() - 6], b'+' | b'-')
    && bytes[bytes.len() - 5].is_ascii_digit()
    && bytes[bytes.len() - 4].is_ascii_digit()
    && bytes[bytes.len() - 3] == b':'
    && bytes[bytes.len() - 2].is_ascii_digit()
    && bytes[bytes.len() - 1].is_ascii_digit();
  if marked {
    let zone = &time[time.len() - 6..];
    let (sign, rest) = zone.split_at(1);
    let magnitude = match rest.split_once(':') {
      Some((hour, minute)) => {
        hour.parse::<i64>().unwrap_or(0) * 60 + minute.parse::<i64>().unwrap_or(0)
      }
      None => 0,
    };
    // The value returned is the zone's offset *from* UTC, so subtracting it turns a local
    // clock reading into UTC: 09:00-05:00 is 14:00Z.
    return (
      &time[..time.len() - 6],
      if sign == "-" { -magnitude } else { magnitude },
    );
  }
  (time.strip_suffix('Z').unwrap_or(time), 0)
}

/// Days from 1970-01-01 to a civil date, using the standard civil-from-days inverse.
fn days_from_epoch(year: i64, month: u32, day: u32) -> Option<i64> {
  if !(1..=12).contains(&month) || day == 0 || day > 31 {
    return None;
  }
  let (year, month) = if month <= 2 {
    (year - 1, month + 9)
  } else {
    (year, month - 3)
  };
  let era = if year >= 0 { year } else { year - 399 } / 400;
  let year_of_era = year - era * 400;
  let day_of_year =
    (153 * month as i64 + 2) / 5 + day as i64 - 1 + 365 * year_of_era + year_of_era / 4;
  Some(era * 146_097 + day_of_year - 719_468)
}

impl PiContent {
  /// The text of one content block.
  pub fn text(&self) -> Option<&str> {
    self.value.get("text").and_then(Value::as_str)
  }
}

/// The events one Pi session maps to, plus the store header to file them under.
#[derive(Debug, Clone)]
pub struct ImportPlan {
  pub header: SessionHeader,
  pub trace_id: String,
  pub events: Vec<Mapped>,
  pub report: ImportReport,
}

/// Pi entry types that produced no event, each with why.
///
/// A skipped entry is not an error: a pi-rs session cannot carry Pi's branch labels, and it
/// is not allowed to carry an extension's injected message as if the user had typed it.
/// Silence is what would be wrong.
#[derive(Debug, Clone, Default)]
pub struct ImportReport {
  /// What each entry type produced, as `"<type>:<role>"` or `"<type>"`.
  pub imported: BTreeMap<String, u32>,
  /// Pi entry kinds that produced nothing, each with how many and the reason.
  pub skipped: BTreeMap<String, Skipped>,
  /// Entries off the active path, by entry type: branches Pi kept that pi-rs does not model.
  pub off_path: BTreeMap<String, u32>,
  /// Content blocks that carried no text.
  pub content: BTreeMap<String, u32>,
  /// The cwd the header was built with, when the file had one.
  pub cwd: Option<String>,
  /// Anything a reader needs to know that is not a count.
  pub notes: Vec<String>,
}

/// One kind of Pi entry that produced no event, and why.
#[derive(Debug, Clone, Default)]
pub struct Skipped {
  pub count: u32,
  pub reason: String,
}

impl ImportReport {
  fn import(&mut self, kind: &str) {
    *self.imported.entry(kind.to_string()).or_insert(0) += 1;
  }

  fn skip(&mut self, kind: &str, reason: &str) {
    let entry = self
      .skipped
      .entry(kind.to_string())
      .or_insert_with(|| Skipped {
        count: 0,
        reason: reason.to_string(),
      });
    entry.count += 1;
  }

  fn content(&mut self, kind: impl Into<String>) {
    *self.content.entry(kind.into()).or_insert(0) += 1;
  }
}

/// One event with the Pi entry it came from.
#[derive(Debug, Clone)]
pub struct Mapped {
  /// Pi's entry id, kept so a report line can point back at the file.
  pub entry_id: String,
  /// Pi's wall-clock time when it wrote the entry.
  pub timestamp_ms: Option<u64>,
  pub event: AgentEvent,
  /// Output text that only the store can place: a tool result's bytes become a blob, and a
  /// blob needs the store's path layout. Planning stays pure without losing the output.
  pub payload: Option<String>,
}

/// An info diagnostic, which is what every "Pi recorded this, pi-rs states it" note is.
fn info(message: String) -> AgentEvent {
  AgentEvent::Diagnostic(Diagnostic {
    level: DiagnosticLevel::Info,
    message,
  })
}

/// The last assistant model seen, and the provider that served it.
#[derive(Debug, Clone, Default)]
struct PiModel {
  provider: Option<String>,
  model: Option<String>,
}

/// A message's content blocks. `content` is a bare string or an array of typed blocks.
#[derive(Debug, Clone)]
pub struct PiContent {
  pub value: Value,
}

impl PiEntry {
  /// A message's role; entries that are not messages have none.
  pub fn message_role(&self) -> Option<&str> {
    self
      .value
      .get("message")
      .and_then(|message| message.get("role"))
      .and_then(Value::as_str)
      .or(self.value.get("role").and_then(Value::as_str))
  }

  /// A field of the nested `message` object, read as text.
  pub fn message_field(&self, key: &str) -> Option<&str> {
    self
      .value
      .get("message")
      .and_then(|message| message.get(key))
      .and_then(Value::as_str)
  }

  /// A field of the nested `message` object, read as a boolean.
  ///
  /// Pi writes flags as JSON booleans; an older file may carry the string. Both are read,
  /// and anything else is `false`, which for `isError` means the conservative-looking
  /// direction: an unrecognised flag records a result as successful, and the raw entry is
  /// still in Pi's file next to the import for whoever needs the difference.
  pub fn message_flag(&self, key: &str) -> bool {
    self
      .value
      .get("message")
      .and_then(|message| message.get(key))
      .and_then(|value| {
        value
          .as_bool()
          .or_else(|| value.as_str().and_then(|text| text.parse::<bool>().ok()))
      })
      .unwrap_or(false)
  }

  /// A field of the nested `message` object, read as an array.
  pub fn message_field_array(&self, key: &str) -> Option<&Vec<Value>> {
    self
      .value
      .get("message")
      .and_then(|message| message.get(key))
      .and_then(Value::as_array)
  }

  /// A field of the nested `message` object, read as an object.
  pub fn message_field_object(&self, key: &str) -> Option<Value> {
    self
      .value
      .get("message")
      .and_then(|message| message.get(key))
      .filter(|value| value.is_object())
      .cloned()
  }

  /// A top-level array field.
  pub fn arr(&self, key: &str) -> Option<&Vec<Value>> {
    self.value.get(key).and_then(Value::as_array)
  }
}

/// The text and attachment count a message's content carried.
#[derive(Debug, Clone, Default)]
struct Extracted {
  text: String,
  attachments: u32,
}

impl Extracted {
  fn non_empty(self) -> Option<String> {
    (!self.text.is_empty()).then_some(self.text)
  }
}

/// The text a Pi message carried, in block order, with the non-text blocks counted.
///
/// Pi stores `content` as a bare string or as typed blocks. Text blocks join with a blank
/// line; a block that carried no text is counted by type, because a message that silently
/// lost an image reads as if it never had one.
fn message_parts(source: &PiSession, entry: &PiEntry, report: &mut ImportReport) -> Extracted {
  let mut extracted = match entry.value.get("message").and_then(|m| m.get("content")) {
    Some(Value::String(text)) => Extracted {
      text: text.clone(),
      attachments: 0,
    },
    Some(Value::Array(blocks)) => content_text(source, report, blocks),
    _ => Extracted::default(),
  };
  // A truncated entry says how much Pi dropped. The fact goes into the text because pi-rs
  // has no other field a reader of the transcript would look at.
  if let Some(dropped) = entry.num("truncated") {
    extracted.text = format!(
      "{}\n[{} bytes truncated by Pi and not stored]",
      extracted.text, dropped
    );
    report.content("truncated by Pi");
  }
  extracted
}

/// The text inside content blocks, counting the blocks that carried none.
fn content_text(source: &PiSession, report: &mut ImportReport, blocks: &[Value]) -> Extracted {
  let mut parts: Vec<String> = Vec::new();
  let mut attachments = 0u32;
  for block in blocks {
    match block.get("type").and_then(Value::as_str) {
      Some("text") => {
        if let Some(text) = block.get("text").and_then(Value::as_str) {
          if !text.is_empty() {
            parts.push(text.to_string());
          }
        }
      }
      // Reasoning inside `content` is not message prose; the assistant arm emits it as its
      // own event, so it is neither counted as an attachment nor joined into the text.
      Some(kind) if is_thinking_kind(kind) => {}
      // A call is neither prose nor an attachment: the caller maps it to `ToolRequested`,
      // and counting it here too would report one thing as both imported and left out.
      Some("toolCall") => {}
      other => {
        attachments += 1;
        report.content(other.unwrap_or("block without a type"));
      }
    }
  }
  let _ = source;
  Extracted {
    text: parts.join("\n\n"),
    attachments,
  }
}

/// Turn a Pi session into pi-rs events.
///
/// Each mapping is a translation, and the comments name what it loses:
///
/// * One assistant message carried a whole request, so it becomes a
///   started/delta/requested/completed span. Pi's usage, stop reason, and model stamp the
///   completion; the context estimate stays `0` because Pi never recorded what it sent.
/// * Pi's stored `thinking` was produced by the model but never streamed to pi-rs, so it is
///   `ProviderSummary`, never `Native`. pi-rs did not see the native channel.
/// * A `model_change` becomes an info diagnostic rather than a `ModelEpochStarted`, because
///   opening an epoch would claim capabilities Pi never recorded.
/// * A tool call is marked `read_only: false`. pi-rs cannot re-run Pi's tools, and the safe
///   direction to be wrong in is the one that asks before executing.
/// * A compaction summary's text is not re-imported: that would put one conversation into
///   the history twice. Only the fact that a boundary existed is recorded.
pub fn plan(source: &PiSession) -> Result<ImportPlan, PiImportError> {
  let active = source.active_path()?;
  let mut report = ImportReport::default();
  let mut events: Vec<Mapped> = Vec::with_capacity(active.len());
  let mut model = PiModel::default();
  let mut requests = 0u32;

  for entry in &active {
    let timestamp_ms = entry.timestamp_ms();
    let mut push = |event: AgentEvent, payload: Option<String>| {
      events.push(Mapped {
        entry_id: entry.id.clone(),
        timestamp_ms,
        event,
        payload,
      });
    };
    match entry.entry_type.as_str() {
      "message" => match entry.message_role().unwrap_or("") {
        "user" => {
          report.import("message:user");
          let extracted = message_parts(source, entry, &mut report);
          push(
            AgentEvent::UserMessage(UserMessage {
              text: extracted.text,
              attachments: extracted.attachments,
            }),
            None,
          );
        }
        "assistant" => {
          report.import("message:assistant");
          requests += 1;
          if let Some(provider) = entry
            .str("provider")
            .or(entry.message_field("provider"))
            .filter(|text| !text.is_empty())
          {
            model.provider = Some(provider.to_string());
          }
          if let Some(name) = entry
            .str("model")
            .or(entry.message_field("model"))
            .filter(|text| !text.is_empty())
          {
            model.model = Some(name.to_string());
          }
          let model_ref = ModelRef::new(
            model
              .provider
              .clone()
              .unwrap_or_else(|| "unknown".to_string()),
            model.model.clone().unwrap_or_else(|| "unknown".to_string()),
          );
          push(
            AgentEvent::ModelRequestStarted(ModelRequestStarted {
              epoch: 0,
              context_tokens_est: 0,
              tools_exposed: 0,
              message_count: requests,
              model: model_ref.clone(),
            }),
            None,
          );
          let mut saw_reasoning = false;
          // Older Pi files kept reasoning in a top-level array; current ones put thinking
          // blocks inside `content`. Both are read, in the order the model produced them.
          for name in ["thinking", "reasoning"] {
            if let Some(text) = entry
              .arr(name)
              .map(|blocks| thinking_text(blocks))
              .filter(|text: &String| !text.is_empty())
            {
              saw_reasoning = true;
              push(
                AgentEvent::ReasoningDelta(ReasoningDelta {
                  text,
                  provenance: ReasoningProvenance::ProviderSummary,
                  chunk_index: 0,
                }),
                None,
              );
            }
          }
          let blocks = entry
            .message_field_array("content")
            .cloned()
            .unwrap_or_default();
          for block in blocks.iter().filter(|block| is_thinking(block)) {
            let text = thinking_text(std::slice::from_ref(block));
            if !text.is_empty() {
              saw_reasoning = true;
              push(
                AgentEvent::ReasoningDelta(ReasoningDelta {
                  text,
                  provenance: ReasoningProvenance::ProviderSummary,
                  chunk_index: 0,
                }),
                None,
              );
            }
          }
          let extracted = content_text(source, &mut report, &blocks);
          if let Some(text) = extracted.clone().non_empty() {
            push(
              AgentEvent::AssistantDelta(AssistantDelta {
                text,
                chunk_index: 0,
              }),
              None,
            );
          }
          let mut tool_calls = 0u32;
          for block in blocks.iter() {
            if block.get("type").and_then(Value::as_str) != Some("toolCall") {
              continue;
            }
            let Some(name) = block.get("name").and_then(Value::as_str) else {
              report.content("tool call without a name");
              continue;
            };
            tool_calls += 1;
            report.import("toolCall");
            // A call Pi did not id gets a derived one so that the lifecycle still has a
            // durable key; the derivation is from the entry, so it is stable across reruns.
            let fallback_id = format!("{}-{}", entry.id, tool_calls);
            push(
              AgentEvent::ToolRequested(ToolRequested {
                call_id: ToolCallId::from_string(
                  block
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or(&fallback_id),
                ),
                name: name.to_string(),
                arguments: block
                  .get("arguments")
                  .cloned()
                  .unwrap_or_else(|| json_string("Pi recorded no arguments")),
                read_only: false,
              }),
              None,
            );
          }
          let usage = entry.message_field_object("usage");
          let count = |key: &str| {
            usage
              .as_ref()
              .and_then(|u| u.get(key))
              .and_then(Value::as_u64)
          };
          for recorded in ["cacheRead", "cacheWrite", "cost"] {
            if usage
              .as_ref()
              .and_then(|usage| usage.get(recorded))
              .is_some()
            {
              report.content(format!("usage:{recorded}"));
            }
          }
          push(
            AgentEvent::ModelRequestCompleted(ModelRequestCompleted {
              epoch: 0,
              model: model_ref,
              finish_reason: entry.message_field("stopReason").map(String::from),
              input_tokens: count("input"),
              output_tokens: count("output"),
              // Pi stores no per-request wall time; zero states "not recorded" better than
              // the difference between two entry timestamps would.
              duration_ms: 0,
              tool_calls,
              // Pi's reasoning text was never streamed to pi-rs, so the span says which
              // provenance the text it does hold came from. An entry that stored no
              // reasoning gets no provenance at all: naming one would claim a reasoning
              // channel for a request the file says nothing about.
              reasoning_provenance: saw_reasoning.then_some(ReasoningProvenance::ProviderSummary),
            }),
            None,
          );
        }
        "toolResult" => {
          report.import("message:toolResult");
          let extracted = message_parts(source, entry, &mut report);
          let call_id = ToolCallId::from_string(
            entry
              .message_field("toolCallId")
              .filter(|text| !text.is_empty())
              .unwrap_or("pi-import-without-id"),
          );
          let name = entry
            .message_field("toolName")
            .filter(|text| !text.is_empty())
            .unwrap_or("unknown")
            .to_string();
          if entry.message_flag("isError") {
            push(
              AgentEvent::ToolFailed(ToolFailed {
                call_id,
                name,
                message: extracted.text,
                duration_ms: 0,
                status: None,
              }),
              None,
            );
          } else {
            let text = extracted.text;
            push(
              AgentEvent::ToolCompleted(ToolCompleted {
                call_id,
                name,
                state: ToolExecutionState::Succeeded,
                duration_ms: 0,
                status: None,
                // The store decides inline vs blob when the payload is filed; write() fills
                // these in, because only it knows the configured threshold.
                reduced: false,
                blob: None,
                visible_bytes: text.len() as u64,
              }),
              Some(text),
            );
          }
        }
        role => report.skip("message", &format!("role '{role}' is not one pi-rs has")),
      },
      "model_change" => {
        report.import("model_change");
        let provider = entry.str("provider").unwrap_or("unknown");
        let to = entry.str("toModelId").unwrap_or("unknown");
        model.provider = Some(provider.to_string());
        model.model = Some(to.to_string());
        // Not a ModelEpochStarted: an epoch opening claims capabilities, and Pi recorded
        // none. The model the header ends up with is still this one.
        push(
          info(format!(
            "imported from Pi: the model changed to {provider}/{to}"
          )),
          None,
        );
      }
      "thinking_level_change" => {
        report.import("thinking_level_change");
        push(
          info(format!(
            "imported from Pi: thinking level set to {}",
            entry.str("thinkingLevel").unwrap_or("unknown")
          )),
          None,
        );
      }
      "compaction" => {
        report.skip(
          "compaction",
          "the boundary is kept; the summary text would put the conversation in twice",
        );
        push(
          info(format!(
            "imported from Pi: a compaction boundary (first kept entry {})",
            entry.str("firstKeptEntryId").unwrap_or("unknown")
          )),
          None,
        );
      }
      "branch_summary" => report.skip(
        "branch_summary",
        "a pi-rs session is linear, so a branch summary has no entry to attach to",
      ),
      "label" => report.skip("label", "a Pi label has no pi-rs equivalent"),
      custom if custom.starts_with("custom") => report.skip(
        custom,
        "extension-owned content would be imported as if the user or model had written it",
      ),
      "session" | "more" => {}
      other => report.skip(other, "no pi-rs event corresponds to this entry type"),
    }
  }

  for entry in source.off_path(&active) {
    *report.off_path.entry(entry.entry_type.clone()).or_insert(0) += 1;
  }
  let dropped: u32 = report.off_path.values().sum();
  if dropped > 0 {
    report.notes.push(format!(
      "{dropped} {} not on the path from the newest entry written",
      if dropped == 1 {
        "entry was"
      } else {
        "entries were"
      }
    ));
  }
  for line in &source.damaged_lines {
    report.notes.push(format!("damaged line skipped: {line}"));
  }

  let started_at_ms = active
    .iter()
    .filter_map(|entry| entry.timestamp_ms())
    .next()
    .or_else(|| is_to_millis(&source.header.timestamp))
    .unwrap_or(0);
  if started_at_ms == 0 {
    report
      .notes
      .push("no entry carried a readable timestamp; the header says 0".to_string());
  }
  let mut header = SessionHeader {
    session_id: SessionId::from_string(pi_session_id(&source.header.id)),
    version: SESSION_SCHEMA_VERSION,
    started_at_ms,
    working_dir: source.header.cwd.clone(),
    model: match (model.provider, model.model) {
      (Some(provider), Some(model)) => ModelRef::new(provider, model),
      _ => {
        report
          .notes
          .push("no assistant model was recorded; the header says unknown/unknown".to_string());
        ModelRef::new("unknown", "unknown")
      }
    },
    parent_session: source
      .header
      .parent_session
      .as_ref()
      .map(|id| SessionId::from_string(id.clone())),
    branched_from_event: None,
    imported_from: Some("pi".to_string()),
  };
  if !source.header.cwd.is_empty() {
    report.cwd = Some(source.header.cwd.clone());
  }
  if header.working_dir.is_empty() {
    header.working_dir = ".".to_string();
    report
      .notes
      .push("Pi recorded no cwd; the header says the current directory".to_string());
  }

  Ok(ImportPlan {
    header,
    trace_id: format!("pi-{}", source.header.id),
    events,
    report,
  })
}

/// The store session id for an imported Pi session. A path component, so only safe pieces.
fn pi_session_id(pi_id: &str) -> String {
  let safe: String = pi_id
    .chars()
    .map(|c| {
      if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
        c
      } else {
        '-'
      }
    })
    .collect();
  let trimmed = safe.trim_matches('-');
  if trimmed.is_empty() {
    "pi-import".to_string()
  } else {
    format!("pi-{trimmed}")
  }
}

/// File the plan as a new session and return the store's session id.
///
/// Imported tool output is filed as a recovery blob whatever its size. pi-rs keeps a tool
/// *result* in the session's message log, which an import does not write — and an inline
/// payload has no field on `ToolCompleted` to live in. A store configured to keep small
/// payloads inline must not be able to silently drop an imported output.
///
/// The bytes pass through the store's durable redaction policy, exactly as bytes pi-rs
/// produced itself would. `reduced` stays what the plan mapped: pi-rs reduced nothing, so it
/// must not claim to have.
pub fn write(store: &Store, plan: &ImportPlan) -> Result<SessionId, StoreError> {
  let mut session = store.begin(plan.header.clone())?;
  let session_id = session.id().clone();
  let trace_id = TraceId::from_string(plan.trace_id.clone());
  let mut emitted = 0usize;
  for mapped in &plan.events {
    let mut event = mapped.event.clone();
    if let Some(text) = &mapped.payload {
      let blob = session.put_recovery_blob(text.as_bytes())?;
      if let AgentEvent::ToolCompleted(completed) = &mut event {
        completed.blob = Some(blob);
      }
    }
    let mut meta = EventMeta::new(session_id.clone(), trace_id.clone());
    meta.model_epoch = Some(0);
    if let Some(timestamp) = mapped.timestamp_ms {
      meta.timestamp_ms = timestamp;
    }
    let mut envelope = EventEnvelope::new(meta, event);
    session.emit(&mut envelope)?;
    emitted += 1;
  }
  // A session that imported nothing still needs the one event that says so, or a reader
  // listing sessions cannot tell an empty import from a missing one.
  if emitted == 0 {
    let meta = EventMeta::new(session_id.clone(), trace_id);
    session.emit(&mut EventEnvelope::new(
      meta,
      info("imported from Pi with no importable entries".to_string()),
    ))?;
  }
  session.finish()?;
  Ok(session_id)
}

/// Whether a content block carries reasoning rather than prose.
fn is_thinking(block: &Value) -> bool {
  block
    .get("type")
    .and_then(Value::as_str)
    .is_some_and(is_thinking_kind)
}

fn is_thinking_kind(kind: &str) -> bool {
  kind == "thinking" || kind == "reasoning"
}

/// The text of reasoning blocks. Pi writes `thinking`, some files write `text`.
fn thinking_text(blocks: &[Value]) -> String {
  let parts: Vec<String> = blocks
    .iter()
    .filter(|block| is_thinking(block))
    .filter_map(|block| {
      block
        .get("thinking")
        .or_else(|| block.get("text"))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(String::from)
    })
    .collect();
  parts.join("\n\n")
}

fn json_string(text: &str) -> Value {
  Value::String(text.to_string())
}

fn string_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
  value.get(key).and_then(Value::as_str)
}

fn number_field(value: &Value, key: &str) -> Option<u64> {
  value.get(key).and_then(Value::as_u64)
}

#[cfg(test)]
mod tests {
  use super::*;

  const HEADER: &str = concat!(
    r#"{"type":"session","version":3,"id":"abc","#,
    r#""timestamp":"2024-12-03T14:00:00.000Z","cwd":"/work/project"}"#
  );

  fn parse_ok(text: &str) -> PiSession {
    parse("test.jsonl", text).expect("fixture parses")
  }

  #[test]
  fn a_file_that_does_not_start_with_a_header_is_not_a_session() {
    let error = parse("x.jsonl", r#"{"type":"message","id":"a","parentId":null}"#).unwrap_err();
    assert!(matches!(error, PiImportError::MissingHeader { .. }));
    assert!(
      error
        .to_string()
        .contains("the first line is type 'message'")
    );
  }

  #[test]
  fn version_one_sessions_without_ids_keep_file_order() {
    let source = parse_ok(concat!(
      r#"{"type":"session","version":1,"id":"old","cwd":"/w"}"#,
      "\n",
      r#"{"type":"message","message":{"role":"user","content":"one"}}"#,
      "\n",
      r#"{"type":"message","message":{"role":"user","content":"two"}}"#
    ));
    let active = source.active_path().unwrap();
    assert_eq!(active.len(), 2);
    assert_eq!(source.header.version, 1);
  }

  #[test]
  fn only_the_path_from_the_newest_entry_written_is_active() {
    let source = parse_ok(&format!(
      "{}\n{}\n{}\n{}",
      HEADER,
      r#"{"type":"message","id":"u1","parentId":null,"message":{"role":"user","content":"start"}}"#,
      r#"{"type":"message","id":"a1","parentId":"u1","message":{"role":"assistant","content":"kept","provider":"p","model":"m"}}"#,
      r#"{"type":"branch_summary","id":"b1","parentId":"u1","fromId":"a1","summary":"abandoned path"}"#
    ));
    let active = source.active_path().unwrap();
    let ids: Vec<&str> = active.iter().map(|entry| entry.id.as_str()).collect();
    // b1 was written last, so Pi's own cursor is on it and a1's branch is the one left out.
    // Following the rule rather than which branch looks more useful is the whole point.
    assert_eq!(ids, vec!["u1", "b1"]);
    assert_eq!(source.off_path(&active).len(), 1);
  }

  #[test]
  fn a_parent_the_file_does_not_contain_is_an_error_not_a_guess() {
    let source = parse_ok(&format!(
      "{}\n{}",
      HEADER,
      r#"{"type":"message","id":"a1","parentId":"missing","message":{"role":"user","content":"orphan"}}"#
    ));
    assert!(matches!(
      source.active_path().unwrap_err(),
      PiImportError::DanglingParent { .. }
    ));
  }

  #[test]
  fn a_parent_cycle_is_reported_rather_than_looped_forever() {
    let source = parse_ok(&format!(
      "{}\n{}\n{}",
      HEADER,
      r#"{"type":"message","id":"a1","parentId":"a2","message":{"role":"user","content":"a"}}"#,
      r#"{"type":"message","id":"a2","parentId":"a1","message":{"role":"user","content":"b"}}"#
    ));
    assert_eq!(
      source.active_path().unwrap_err(),
      PiImportError::Cycle { id: "a2".into() }
    );
  }

  #[test]
  fn two_entries_sharing_an_id_have_no_defined_tree() {
    let source = parse_ok(&format!(
      "{}\n{}\n{}",
      HEADER,
      r#"{"type":"message","id":"dup","parentId":null,"message":{"role":"user","content":"a"}}"#,
      r#"{"type":"message","id":"dup","parentId":null,"message":{"role":"user","content":"b"}}"#
    ));
    assert_eq!(
      source.active_path().unwrap_err(),
      PiImportError::DuplicateId { id: "dup".into() }
    );
  }

  #[test]
  fn damaged_lines_are_skipped_and_named() {
    let source = parse_ok(&format!(
      "{}\nnot json\n{}",
      HEADER, r#"{"type":"label","id":"l1","parentId":null,"targetId":"x","label":"keep"}"#
    ));
    assert_eq!(source.damaged_lines.len(), 1);
    assert!(source.damaged_lines[0].starts_with("line 2:"));
  }

  #[test]
  fn pi_timestamps_become_epoch_milliseconds() {
    assert_eq!(
      is_to_millis("2024-12-03T14:00:00.000Z"),
      Some(1_733_234_400_000)
    );
    assert_eq!(
      is_to_millis("2024-12-03T09:00:00-05:00"),
      Some(1_733_234_400_000)
    );
    assert_eq!(
      is_to_millis("2024-12-03T14:00:00.250Z"),
      Some(1_733_234_400_250)
    );
    // A stamp with no zone is read as UTC rather than dropped, so order survives.
    assert_eq!(is_to_millis("2024-12-03T14:00:00"), Some(1_733_234_400_000));
    assert_eq!(
      is_to_millis("2024-12-03T14:00:37.500Z"),
      Some(1_733_234_437_500)
    );
    assert_eq!(is_to_millis("yesterday"), None);
    assert_eq!(is_to_millis("2024-13-03T14:00:00Z"), None);
  }

  fn events(plan: &ImportPlan) -> Vec<&AgentEvent> {
    plan.events.iter().map(|event| &event.event).collect()
  }

  #[test]
  fn one_assistant_message_becomes_a_whole_request_span() {
    let source = parse_ok(&format!(
      "{}\n{}\n{}",
      HEADER,
      r#"{"type":"message","id":"u1","parentId":null,"timestamp":"2024-12-03T14:00:01.000Z","message":{"role":"user","content":"go"}}"#,
      r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2024-12-03T14:00:02.000Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"mulling"},{"type":"text","text":"doing it"},{"type":"toolCall","id":"call_1","name":"bash","arguments":{"command":"ls"}}],"provider":"anthropic","model":"claude","usage":{"input":120,"output":30,"cacheRead":900},"stopReason":"toolUse"}}"#
    ));
    let plan = plan(&source).unwrap();
    let events = events(&plan);
    assert!(matches!(events[0], AgentEvent::UserMessage(_)));
    assert!(matches!(events[1], AgentEvent::ModelRequestStarted(_)));
    let AgentEvent::ReasoningDelta(reasoning) = events[2] else {
      panic!(
        "expected the thinking block to become a reasoning delta, got {:?}",
        events[2]
      );
    };
    assert_eq!(reasoning.provenance, ReasoningProvenance::ProviderSummary);
    let AgentEvent::AssistantDelta(delta) = events[3] else {
      panic!("expected assistant prose");
    };
    assert_eq!(delta.text, "doing it");
    let AgentEvent::ToolRequested(requested) = events[4] else {
      panic!("expected a tool request");
    };
    assert!(!requested.read_only);
    let AgentEvent::ModelRequestCompleted(completed) = events[5] else {
      panic!("expected the request to close");
    };
    assert_eq!(completed.input_tokens, Some(120));
    assert_eq!(completed.finish_reason.as_deref(), Some("toolUse"));
    assert_eq!(completed.tool_calls, 1);
    assert_eq!(
      completed.reasoning_provenance,
      Some(ReasoningProvenance::ProviderSummary)
    );
    assert_eq!(plan.header.model, ModelRef::new("anthropic", "claude"));
    assert_eq!(plan.header.imported_from.as_deref(), Some("pi"));
    assert_eq!(plan.header.working_dir, "/work/project");
    assert_eq!(plan.header.started_at_ms, 1_733_234_401_000);
    // Pi recorded cache tokens that pi-rs's event has no field for, so the report says so.
    assert_eq!(plan.report.content.get("usage:cacheRead"), Some(&1));
  }

  #[test]
  fn images_are_counted_not_dropped_silently() {
    let source = parse_ok(&format!(
      "{}\n{}",
      HEADER,
      r#"{"type":"message","id":"u1","parentId":null,"message":{"role":"user","content":[{"type":"text","text":"look"},{"type":"image","data":"AAA","mimeType":"image/png"}]}}"#
    ));
    let plan = plan(&source).unwrap();
    let AgentEvent::UserMessage(message) = &plan.events[0].event else {
      panic!("expected a user message");
    };
    assert_eq!(message.text, "look");
    assert_eq!(message.attachments, 1);
    assert_eq!(plan.report.content.get("image"), Some(&1));
  }

  #[test]
  fn what_pi_cannot_carry_is_reported_with_a_reason() {
    let source = parse_ok(&format!(
      "{}\n{}\n{}\n{}\n{}",
      HEADER,
      r#"{"type":"message","id":"u1","parentId":null,"message":{"role":"user","content":"hi"}}"#,
      r#"{"type":"custom","id":"c1","parentId":"u1","customType":"ext","data":{"n":1}}"#,
      r#"{"type":"custom_message","id":"cm1","parentId":"c1","customType":"ext","content":"injected","display":true}"#,
      r#"{"type":"label","id":"l1","parentId":"cm1","targetId":"u1","label":"checkpoint"}"#
    ));
    let plan = plan(&source).unwrap();
    assert_eq!(plan.report.imported.get("message:user"), Some(&1));
    for kind in ["custom", "custom_message", "label"] {
      assert!(plan.report.skipped.contains_key(kind), "missing {kind}");
    }
    assert!(
      plan.report.skipped["custom_message"]
        .reason
        .contains("as if the user")
    );
  }

  #[test]
  fn entries_left_off_a_branch_are_counted_by_type() {
    let source = parse_ok(&format!(
      "{}\n{}\n{}\n{}",
      HEADER,
      r#"{"type":"message","id":"u1","parentId":null,"timestamp":"2024-12-03T14:00:01.000Z","message":{"role":"user","content":"start"}}"#,
      r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2024-12-03T14:00:05.000Z","message":{"role":"assistant","content":"kept","provider":"p","model":"m"}}"#,
      r#"{"type":"custom","id":"c1","parentId":"u1","customType":"ext","data":{"n":1}}"#
    ));
    let plan = plan(&source).unwrap();
    // c1 is the newest entry written, so a1's branch is the one dropped; the report names
    // its type and count instead of leaving a silently shorter session.
    assert_eq!(plan.report.off_path.get("message").copied(), Some(1));
    assert!(
      plan
        .report
        .notes
        .iter()
        .any(|note| note.contains("newest entry written"))
    );
  }

  #[test]
  fn a_session_with_no_model_names_unknown_instead_of_guessing() {
    let source = parse_ok(&format!(
      "{}\n{}",
      HEADER,
      r#"{"type":"message","id":"u1","parentId":null,"message":{"role":"user","content":"only a question"}}"#
    ));
    let plan = plan(&source).unwrap();
    assert_eq!(plan.header.model, ModelRef::new("unknown", "unknown"));
    assert!(
      plan
        .report
        .notes
        .iter()
        .any(|note| note.contains("unknown/unknown"))
    );
  }

  #[test]
  fn a_compaction_boundary_is_recorded_without_its_summary_text() {
    let source = parse_ok(&format!(
      "{}\n{}\n{}",
      HEADER,
      r#"{"type":"message","id":"u1","parentId":null,"message":{"role":"user","content":"older"}}"#,
      r#"{"type":"compaction","id":"k1","parentId":"u1","summary":"a long summary of older turns","firstKeptEntryId":"u1"}"#
    ));
    let plan = plan(&source).unwrap();
    let text: String = plan
      .events
      .iter()
      .map(|mapped| match &mapped.event {
        AgentEvent::Diagnostic(diagnostic) => diagnostic.message.clone(),
        AgentEvent::UserMessage(message) => message.text.clone(),
        _ => String::new(),
      })
      .collect();
    assert!(text.contains("compaction boundary"));
    assert!(!text.contains("a long summary"));
    assert_eq!(plan.report.skipped.get("compaction").unwrap().count, 1);
  }
}
