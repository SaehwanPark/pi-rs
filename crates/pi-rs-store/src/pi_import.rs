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
  AgentEvent, AssistantDelta, ContentBlock, Diagnostic, DiagnosticLevel, EventEnvelope, EventMeta,
  Message, ModelRef, ModelRequestCompleted, ModelRequestStarted, ReasoningDelta,
  ReasoningProvenance, Role, SessionHeader, SessionId, ToolCallBlock, ToolCallId, ToolCompleted,
  ToolExecutionState, ToolFailed, ToolRequested, ToolResultBlock, TraceId, TurnId, UserMessage,
  session::SESSION_SCHEMA_VERSION,
};

use crate::{Payload, Store, StoreError};

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
  // The `- year_of_era / 100` term is the century correction from `days_from_civil`: century
  // years divisible by 100 are not leap years, and leaving it out shifts every date whose
  // `year_of_era` reaches 100 (1701-2000, and 2101-2400) by one to three days.
  let day_of_era = 365 * year_of_era + year_of_era / 4 - year_of_era / 100
    + (153 * month as i64 + 2) / 5
    + day as i64
    - 1;
  Some(era * 146_097 + day_of_era - 719_468)
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
  /// Conversation messages recorded in the session log, which is what a resume would see.
  ///
  /// This can be fewer than the imported entries: not every entry pi-rs can describe as an
  /// event is also a message a model would be shown.
  pub messages: u32,
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
  /// Line of the file the entry was read from, for the same reason.
  pub line: usize,
  /// Pi's wall-clock time when it wrote the entry.
  pub timestamp_ms: Option<u64>,
  pub event: AgentEvent,
  /// Output text that only the store can place: whether a tool result's bytes become a blob
  /// depends on the store's inline threshold, and a blob needs its path layout. Planning stays
  /// pure without losing the output.
  pub payload: Option<String>,
  /// The turn the event belongs to. Pi's file has no turn ids, so the import anchors one on
  /// the user entry that opened the turn; it is stable across re-imports.
  pub turn_id: Option<TurnId>,
  /// A conversation message this event introduces, which the store records in the session log
  /// bound to this event. Binding to the introducing event is what lets a resume rebuild the
  /// conversation without matching ids, exactly as for a native session.
  pub message: Option<MappedMessage>,
}

/// A message to record, and the model to attribute it to.
#[derive(Debug, Clone)]
pub struct MappedMessage {
  pub message: Message,
  pub model: ModelRef,
}

/// The calls as message blocks, in the order the model asked for them.
fn call_blocks(calls: &[ToolCallBlock]) -> Vec<ContentBlock> {
  calls
    .iter()
    .map(|call| ContentBlock::ToolCall(call.clone()))
    .collect()
}

/// One message record, or none when the entry carried nothing a message would hold.
///
/// An empty message is not recorded because a resume would carry a blank turn into the next
/// request. The trace still holds what the entry was, and the report still counts it.
fn message_record(extracted: &Extracted, role: Role, model: &ModelRef) -> Option<MappedMessage> {
  if extracted.blocks.is_empty() {
    return None;
  }
  Some(MappedMessage {
    message: Message::new(role, extracted.blocks.clone()),
    model: model.clone(),
  })
}

/// The model in effect for an entry that does not name one of its own.
///
/// A user message has no model, but a message record is attributed to one, so the import gives
/// it the last assistant model recorded *before* it. Before the first assistant entry the file
/// has named no model, and `unknown` is the honest answer: what the user had configured is not
/// in the file, and naming one would attribute a message to a model the file never showed.
fn current_model(model: &PiModel) -> ModelRef {
  let named = |recorded: &Option<String>| {
    recorded
      .clone()
      .filter(|text| !text.is_empty())
      .unwrap_or_else(|| "unknown".to_string())
  };
  ModelRef::new(named(&model.provider), named(&model.model))
}

/// The id a turn is anchored on: Pi's entry id when the file has ids, its line otherwise.
fn turn_anchor(entry: &PiEntry) -> String {
  if entry.id.is_empty() {
    format!("line-{}", entry.line)
  } else {
    entry.id.clone()
  }
}

/// An info diagnostic, which is what every "Pi recorded this, pi-rs states it" note is.
fn info(message: String) -> AgentEvent {
  AgentEvent::Diagnostic(Diagnostic {
    level: DiagnosticLevel::Info,
    message,
  })
}

/// A turn anchor for an event whose entry had no opening user message: it anchors its own turn.
fn anchor_of(mapped: &Mapped) -> String {
  if mapped.entry_id.is_empty() {
    format!("line-{}", mapped.line)
  } else {
    mapped.entry_id.clone()
  }
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
  /// The same content as pi-rs's own blocks, for the session log's message record.
  ///
  /// The trace records a message's prose in one event; the log keeps the block structure a
  /// model request needs. Deriving both from one pass is what stops the log claiming a block
  /// the trace never counted, or a count the log has nothing behind.
  blocks: Vec<ContentBlock>,
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
      blocks: text_blocks(text),
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
  let mut owned: Vec<ContentBlock> = Vec::new();
  let mut attachments = 0u32;
  for block in blocks {
    match block.get("type").and_then(Value::as_str) {
      Some("text") => {
        if let Some(text) = block.get("text").and_then(Value::as_str) {
          if !text.is_empty() {
            parts.push(text.to_string());
            owned.push(ContentBlock::Text {
              text: text.to_string(),
            });
          }
        }
      }
      // Reasoning inside `content` is not message prose; the assistant arm emits it as its
      // own event, so it is neither counted as an attachment nor joined into the text.
      Some(kind) if is_thinking_kind(kind) => {}
      // A call is neither prose nor an attachment: the caller maps it to `ToolRequested`,
      // and counting it here too would report one thing as both imported and left out.
      Some("toolCall") => {}
      // An image carries bytes, so the session log can hold it the way a native session does:
      // inline, as an image block. One with no data cannot be held, so it stays a counted and
      // reported attachment instead of a block with nothing in it.
      Some("image") => {
        attachments += 1;
        match block.get("data").and_then(Value::as_str) {
          Some(data) => owned.push(ContentBlock::Image {
            mime: block
              .get("mimeType")
              .and_then(Value::as_str)
              .unwrap_or("image/png")
              .to_string(),
            data_base64: data.to_string(),
          }),
          None => report.content("image without data"),
        }
      }
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
    blocks: owned,
  }
}

/// One text block, or none when the message was a bare empty string.
fn text_blocks(text: &str) -> Vec<ContentBlock> {
  if text.is_empty() {
    Vec::new()
  } else {
    vec![ContentBlock::Text {
      text: text.to_string(),
    }]
  }
}

/// The calls one assistant message asked for, in order, with pi-rs ids.
///
/// A call Pi did not id gets a derived one so that the lifecycle still has a durable key; the
/// derivation is from the entry, so it is stable across reruns.
fn tool_call_blocks(
  entry: &PiEntry,
  blocks: &[Value],
  report: &mut ImportReport,
) -> Vec<ToolCallBlock> {
  let mut calls: Vec<ToolCallBlock> = Vec::new();
  for block in blocks {
    if block.get("type").and_then(Value::as_str) != Some("toolCall") {
      continue;
    }
    let Some(name) = block.get("name").and_then(Value::as_str) else {
      report.content("tool call without a name");
      continue;
    };
    report.import("toolCall");
    calls.push(ToolCallBlock {
      id: ToolCallId::from_string(
        block
          .get("id")
          .and_then(Value::as_str)
          .filter(|text| !text.is_empty())
          .unwrap_or(&format!("{}-{}", entry.id, calls.len() + 1)),
      ),
      name: name.to_string(),
      arguments: block
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json_string("Pi recorded no arguments")),
    });
  }
  calls
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
  // Pi's file groups turns implicitly. The import opens a turn at each user entry and stamps
  // every later entry with it, which is also what makes an imported session resumable: the
  // store's message records are keyed by turn.
  let mut turn: Option<TurnId> = None;
  let mut turns: BTreeMap<usize, Option<TurnId>> = BTreeMap::new();

  for entry in &active {
    let timestamp_ms = entry.timestamp_ms();
    let mut push = |event: AgentEvent, payload: Option<String>, message: Option<MappedMessage>| {
      events.push(Mapped {
        entry_id: entry.id.clone(),
        line: entry.line,
        timestamp_ms,
        event,
        payload,
        turn_id: None,
        message,
      });
    };
    match entry.entry_type.as_str() {
      "message" => match entry.message_role().unwrap_or("") {
        "user" => {
          report.import("message:user");
          // A turn is one user entry plus everything until the next one. Pi records no turn id,
          // so the import anchors one on the entry that opened the turn.
          turn = Some(TurnId::from_string(format!("turn-{}", turn_anchor(entry))));
          let extracted = message_parts(source, entry, &mut report);
          let message = message_record(&extracted, Role::User, &current_model(&model));
          push(
            AgentEvent::UserMessage(UserMessage {
              text: extracted.text.clone(),
              attachments: extracted.attachments,
            }),
            None,
            message,
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
          let blocks = entry
            .message_field_array("content")
            .cloned()
            .unwrap_or_default();
          let extracted = content_text(source, &mut report, &blocks);
          let calls = tool_call_blocks(entry, &blocks, &mut report);
          // Prose and calls together are one assistant message, which is what a resume feeds
          // the next request. Reasoning stays out of it because pi-rs keeps a model's
          // reasoning out of the next request's messages by construction.
          let message = message_record(
            &Extracted {
              blocks: [extracted.blocks.clone(), call_blocks(&calls)].concat(),
              ..extracted.clone()
            },
            Role::Assistant,
            &model_ref,
          );
          // A message binds to the event that introduces it, and for an assistant message that
          // is the first delta. A reply with no prose has no delta, so it binds to the request
          // that produced the calls.
          let (started_message, delta_message) = if extracted.text.is_empty() {
            (message, None)
          } else {
            (None, message)
          };
          push(
            AgentEvent::ModelRequestStarted(ModelRequestStarted {
              epoch: 0,
              context_tokens_est: 0,
              tools_exposed: 0,
              message_count: requests,
              model: model_ref.clone(),
            }),
            None,
            started_message,
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
                None,
              );
            }
          }
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
                None,
              );
            }
          }
          if let Some(text) = extracted.non_empty() {
            push(
              AgentEvent::AssistantDelta(AssistantDelta {
                text,
                chunk_index: 0,
              }),
              None,
              delta_message,
            );
          }
          for call in &calls {
            push(
              AgentEvent::ToolRequested(ToolRequested {
                call_id: call.id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
                // pi-rs cannot re-run Pi's tools, and the safe direction to be wrong in is
                // the one that asks before executing.
                read_only: false,
              }),
              None,
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
              tool_calls: calls.len() as u32,
              // Pi's reasoning text was never streamed to pi-rs, so the span says which
              // provenance the text it does hold came from. An entry that stored no
              // reasoning gets no provenance at all: naming one would claim a reasoning
              // channel for a request the file says nothing about.
              reasoning_provenance: saw_reasoning.then_some(ReasoningProvenance::ProviderSummary),
            }),
            None,
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
          // A tool result is a message of its own, bound to the terminal event, whether or not
          // the call succeeded: a resume needs what the tools answered, including the failures
          // the model then had to work around.
          let failed = entry.message_flag("isError");
          let record = message_record(
            &Extracted {
              blocks: vec![ContentBlock::ToolResult(ToolResultBlock {
                id: call_id.clone(),
                name: name.clone(),
                state: if failed {
                  ToolExecutionState::Failed
                } else {
                  ToolExecutionState::Succeeded
                },
                text: extracted.text.clone(),
                is_error: failed,
                // pi-rs reduced nothing: these are Pi's bytes as Pi stored them.
                reduced: false,
              })],
              ..Default::default()
            },
            Role::Tool,
            &current_model(&model),
          );
          if failed {
            push(
              AgentEvent::ToolFailed(ToolFailed {
                call_id,
                name,
                message: extracted.text,
                duration_ms: 0,
                status: None,
              }),
              None,
              record,
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
              record,
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
    turns.insert(entry.line, turn.clone());
  }
  for mapped in events.iter_mut() {
    mapped.turn_id = turns.get(&mapped.line).cloned().flatten();
  }
  report.messages = events
    .iter()
    .filter(|mapped| mapped.message.is_some())
    .count() as u32;

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
/// Two durable records come out of one plan. Each event goes to the trace journal, and each
/// message the events introduce goes to the session log bound to its introducing event — the
/// binding the store uses to read a conversation back without matching ids. The message log is
/// what makes an imported session resumable; the trace is what makes it inspectable.
///
/// Imported tool output follows the same inline rule as native output: past the store's inline
/// threshold its bytes become a blob, and below it the message record is their only durable
/// copy. The bytes pass through the store's durable redaction policy either way, exactly as
/// bytes pi-rs produced itself would. `reduced` stays what the plan mapped: pi-rs reduced
/// nothing, so it must not claim to have.
pub fn write(store: &Store, plan: &ImportPlan) -> Result<SessionId, StoreError> {
  let mut session = store.begin(plan.header.clone())?;
  let session_id = session.id().clone();
  let trace_id = TraceId::from_string(plan.trace_id.clone());
  let mut emitted = 0usize;
  for mapped in &plan.events {
    let mut event = mapped.event.clone();
    if let Some(text) = &mapped.payload {
      // A tool result's durable copy is its message record, which is what the next request
      // reads, so the trace needs a blob only past the store's inline threshold. That is the
      // rule a native session follows; filing every imported output would keep the same bytes
      // twice for no reason. Nothing is marked reduced because pi-rs reduced nothing: these
      // are Pi's bytes as Pi stored them.
      if let AgentEvent::ToolCompleted(completed) = &mut event {
        if let Payload::Blob(blob) = session.put_payload(text.as_bytes())? {
          completed.blob = Some(blob);
        }
      }
    }
    let mut meta = EventMeta::new(session_id.clone(), trace_id.clone());
    meta.model_epoch = Some(0);
    if let Some(timestamp) = mapped.timestamp_ms {
      meta.timestamp_ms = timestamp;
    }
    let mut envelope = EventEnvelope::new(meta, event);
    session.emit(&mut envelope)?;
    if let Some(record) = &mapped.message {
      // The message record is bound to the event that introduces it, so reading the session
      // back finds it through that binding instead of matching ids. The event has its sequence
      // number by now, which is what the binding points at. Epoch 0 because a Pi file records
      // no failover.
      let turn_id = mapped
        .turn_id
        .clone()
        .unwrap_or_else(|| TurnId::from_string(format!("turn-{}", anchor_of(mapped))));
      session.append_message(&turn_id, &record.message, 0, &record.model, &envelope)?;
    }
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

  /// The century correction inside `days_from_epoch`, which the 21st-century cases above cannot
  /// see: `year_of_era / 100` is zero for every year from 2001 through 2099.
  #[test]
  fn days_from_epoch_carries_the_century_correction() {
    // The epoch itself, three days out whenever the correction is missing.
    assert_eq!(is_to_millis("1970-01-01T00:00:00.000Z"), Some(0));
    assert_eq!(
      is_to_millis("1999-12-31T00:00:00.000Z"),
      Some(946_598_400_000)
    );
    // Both sides of the leap century: 2000 is a leap year because 400 divides it.
    assert_eq!(
      is_to_millis("2000-02-29T12:00:00.000Z"),
      Some(951_825_600_000)
    );
    assert_eq!(
      is_to_millis("2000-03-01T00:00:00.000Z"),
      Some(951_868_800_000)
    );
    // The control, a year where the missing term would have been zero anyway.
    assert_eq!(
      is_to_millis("2024-12-03T14:00:00.000Z"),
      Some(1_733_234_400_000)
    );
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
  fn images_are_carried_where_native_sessions_carry_them() {
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
    // pi-rs keeps an image inline in the session's messages, so an import does too, and the
    // report no longer says the image was left out.
    assert!(!plan.report.content.contains_key("image"));
    let recorded = plan.events[0]
      .message
      .as_ref()
      .expect("the user message is recorded");
    assert!(matches!(
      recorded.message.content[1],
      ContentBlock::Image {
        ref mime,
        ref data_base64
      } if mime == "image/png" && data_base64 == "AAA"
    ));
  }

  #[test]
  fn an_image_without_bytes_is_still_reported() {
    let source = parse_ok(&format!(
      "{}\n{}",
      HEADER,
      r#"{"type":"message","id":"u1","parentId":null,"timestamp":"2026-07-24T09:00:01.000Z","message":{"role":"user","content":[{"type":"image"}]}}"#
    ));
    let plan = plan(&source).unwrap();
    let AgentEvent::UserMessage(message) = &plan.events[0].event else {
      panic!("expected a user message");
    };
    assert_eq!(message.attachments, 1);
    assert_eq!(plan.report.content.get("image without data"), Some(&1));
    assert!(
      plan.events[0].message.is_none(),
      "a block with no bytes has nothing to record"
    );
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
