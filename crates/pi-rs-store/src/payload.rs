//! Keeping one trace line inside its inline budget.
//!
//! The unit a reader pays for is the line. `grep`, a `tail`, a resume that only
//! needs the last few events, a journal copied off a machine: all of them cost
//! bytes per line, so a single 40 KiB tool output makes every later reader pay for
//! bytes that were never going to be read. Above the configured budget the whole
//! value goes to content-addressed storage once, and the line keeps a bounded
//! preview that states how much it left out and where the rest lives.
//!
//! Nothing is lost and nothing is hidden. The bytes are stored verbatim after the
//! redaction the journal already applied, the reference is the same
//! `blobs/<shard>/<hash>` form the rest of the store uses, and the machine-readable
//! [`ExternalizedField`] records travel beside the line so a caller never has to
//! parse prose out of a preview to learn the sizes.
//!
//! Two kinds of field are never spilled, and both refusals are about being able to
//! read the trace at all rather than about size:
//!
//! - the envelope's own bookkeeping, because a line that cannot be attributed to a
//!   session, a sequence position, or an event kind is not a trace line;
//! - identity and pointer-shaped fields (`*_id`, `*_ref`, `hash`, `type`), because
//!   an elided pointer cannot be followed: it would silently break the recovery
//!   path the pointer exists to provide.

use serde_json::Value;

use pi_rs_core::{BlobRef, ExternalizedField};

use crate::{blob::BlobStore, error::StoreError};

/// Bytes of a spilled value that stay inline.
///
/// Small enough that a line stays a line; large enough that the opening of a diff
/// or the first rows of a table identify the payload without fetching it.
pub const PREVIEW_BYTES: usize = 256;

/// Spills to attempt before giving up on a line.
///
/// Each spill is a fresh walk plus a serialization, so bounding the work protects
/// the write path from a line built out of many medium fields, where reaching the
/// budget would take a long search for a small win. Such a line stays over budget,
/// still decodes, and keeps a record of every spill that was made.
const MAX_SPILLS_PER_LINE: usize = 32;

/// Root fields whose whole subtree must survive intact.
///
/// The first group is the envelope's bookkeeping: a line that cannot be attributed
/// to a session, a sequence position, or an event kind is not a trace line. `blob`
/// is a pointer record (`hash`, `relative_path`), and eliding any part of a pointer
/// breaks the thing the pointer was recorded to make recoverable.
const ENVELOPE_FIELDS: &[&str] = &[
  "v",
  "meta",
  "type",
  "redactions",
  "raw_payload",
  "raw_ref",
  "externalized",
  "blob",
];

/// Bound `line` to `budget` bytes, storing whole fields in `blobs`.
///
/// `measure` is the caller's own cost function, so what gets bounded is what
/// actually gets written rather than an approximation of it.
///
/// The largest remaining candidate is spilled first, because it is the only kind of
/// spill that can help: bounding an already-small field costs bytes. Spilling stops
/// when the line fits, when the largest candidate would not shrink, or when nothing
/// may be spilled, or after [`MAX_SPILLS_PER_LINE`] of them. A budget smaller than
/// the bounded form cannot be met, and the line is then left as small as it could be
/// made rather than corrupted to reach it.
///
/// The caller must apply the redaction policy to `line` first: values leave this
/// module as-is, so an unredacted field would reach the blob store unredacted.
pub fn bound(
  line: &mut Value,
  blobs: &BlobStore,
  budget: u64,
  measure: impl Fn(&Value) -> Result<u64, StoreError>,
) -> Result<Vec<ExternalizedField>, StoreError> {
  let mut spilled: Vec<ExternalizedField> = Vec::new();
  for _ in 0..MAX_SPILLS_PER_LINE {
    if measure(line)? <= budget {
      break;
    }
    let done: Vec<&str> = spilled.iter().map(|field| field.field.as_str()).collect();
    let Some(path) = largest_candidate(line, &done) else {
      break;
    };
    let Some(Value::String(text)) = lookup(line, &path) else {
      // The candidate came out of this same line, so a lookup failure means the
      // two walks disagree. Stop rather than guess at what to store.
      break;
    };
    let text = text.clone();
    let preview = preview(&text);
    // The reference is derived from the bytes, so the bounded form is knowable
    // before anything is written. Deciding first means a spill that is rejected
    // for futility does not leave unreferenced bytes behind.
    let reference = BlobRef::for_bytes(text.as_bytes(), None).relative_path();
    let kept = format!(
      "{preview}\u{2026} [stored {} bytes in {}, {} bytes shown]",
      text.len(),
      reference,
      preview.len()
    );
    // A value no longer than its own bounded form cannot be reduced by spilling,
    // and it was the largest candidate, so no other candidate can be either.
    if kept.len() >= text.len() || !replace(line, &path, &kept) {
      break;
    }
    blobs.put(text.as_bytes(), None)?;
    spilled.push(ExternalizedField {
      field: path.join("/"),
      reference,
      bytes: text.len() as u64,
      inline: kept.len() as u64,
    });
  }
  Ok(spilled)
}

/// Leading bytes of a value, cut at a character boundary.
fn preview(text: &str) -> String {
  if text.len() <= PREVIEW_BYTES {
    return text.to_string();
  }
  let mut end = PREVIEW_BYTES;
  while end > 0 && !text.is_char_boundary(end) {
    end -= 1;
  }
  text[..end].to_string()
}

/// The largest spillable string leaf, as a `/`-separated path.
///
/// Ties break on the path, so the same line always spills the same field: a trace
/// whose layout depended on map iteration order could not be diffed or replayed.
fn largest_candidate(line: &Value, done: &[&str]) -> Option<Vec<String>> {
  let mut found: Vec<(usize, Vec<String>)> = Vec::new();
  collect(line, &mut Vec::new(), &mut found);
  found.retain(|(_, path)| spills(path) && !done.contains(&path.join("/").as_str()));
  found.sort_by(|(left_length, left), (right_length, right)| {
    right_length.cmp(left_length).then_with(|| left.cmp(right))
  });
  found.into_iter().next().map(|(_, path)| path)
}

/// Every string leaf with its path, however deeply nested.
fn collect(value: &Value, path: &mut Vec<String>, found: &mut Vec<(usize, Vec<String>)>) {
  match value {
    Value::Object(map) => {
      for (key, inner) in map {
        path.push(key.clone());
        collect(inner, path, found);
        path.pop();
      }
    }
    Value::Array(items) => {
      for (index, inner) in items.iter().enumerate() {
        path.push(index.to_string());
        collect(inner, path, found);
        path.pop();
      }
    }
    Value::String(text) => found.push((text.len(), path.clone())),
    _ => {}
  }
}

/// Whether this path may have its bytes stored out of the line.
fn spills(path: &[String]) -> bool {
  match path.first() {
    // A string with no field name above it has no identity to reason about.
    None => false,
    Some(first) => {
      !ENVELOPE_FIELDS.contains(&first.as_str())
        && !path.iter().last().is_some_and(|leaf| is_pointer(leaf))
    }
  }
}

/// Leaf names that identify something rather than describe it.
///
/// An elided identifier cannot be followed, quoted back, or correlated with
/// another record, and a caller reading a trace cannot tell the difference between
/// an identifier that was bounded away and one that was always that short.
fn is_pointer(field: &str) -> bool {
  field == "type"
    || field == "id"
    || field == "hash"
    || field == "reference"
    || field.ends_with("_id")
    || field.ends_with("_ref")
}

/// The value at `path`, if it is still there.
fn lookup<'a>(line: &'a Value, path: &[String]) -> Option<&'a Value> {
  let mut current = line;
  for segment in path {
    current = match (current, segment.as_str()) {
      (Value::Object(map), key) => map.get(key)?,
      (Value::Array(items), index) => items.get(index.parse::<usize>().ok()?)?,
      _ => return None,
    };
  }
  Some(current)
}

/// Replace the string at `path`. False when the path no longer resolves to one.
fn replace(line: &mut Value, path: &[String], text: &str) -> bool {
  let Some((segment, rest)) = path.split_first() else {
    return false;
  };
  let child: Option<&mut Value> = match (line, segment.as_str()) {
    (Value::Object(map), key) => map.get_mut(key),
    (Value::Array(items), index) => match index.parse::<usize>() {
      Ok(index) => items.get_mut(index),
      Err(_) => None,
    },
    _ => None,
  };
  let Some(child) = child else {
    return false;
  };
  if rest.is_empty() {
    return match child {
      Value::String(_) => {
        *child = Value::String(text.to_string());
        true
      }
      _ => false,
    };
  }
  replace(child, rest, text)
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use pi_rs_core::BlobRef;

  use super::*;
  use crate::{StateLayout, TempDir};

  /// Line cost as the journal would count it: one compact JSON line.
  fn bytes(line: &Value) -> Result<u64, StoreError> {
    Ok(serde_json::to_string(line)?.len() as u64)
  }

  struct Fixture {
    /// Held so the directory outlives the blob store it contains.
    _temp: TempDir,
    blobs: BlobStore,
  }

  fn fixture() -> Result<Fixture, StoreError> {
    let temp = TempDir::new("payload-bounding");
    let layout = StateLayout::new(temp.path());
    let session = pi_rs_core::SessionId::new();
    Ok(Fixture {
      blobs: BlobStore::for_session(&layout, &session)?,
      _temp: temp,
    })
  }

  #[test]
  fn a_line_inside_its_budget_is_left_alone() {
    let fixture = fixture().unwrap();
    let mut line = json!({"type": "diagnostic", "message": "short"});
    let spilled = bound(&mut line, &fixture.blobs, 4096, bytes).unwrap();
    assert!(spilled.is_empty());
    assert_eq!(line, json!({"type": "diagnostic", "message": "short"}));
    assert_eq!(fixture.blobs.bytes().unwrap(), 0, "no bytes for nothing");
  }

  /// A `write` request carries the file contents as a model-generated argument,
  /// which is the ordinary way a line becomes enormous.
  #[test]
  fn a_large_field_is_stored_whole_and_the_line_keeps_a_bounded_preview() {
    let fixture = fixture().unwrap();
    let output = "x".repeat(40 * 1024);
    let mut line = json!({
      "type": "tool_requested",
      "call_id": "call_1",
      "name": "write",
      "arguments": {"path": "generated/data.txt", "contents": output},
    });
    let spilled = bound(&mut line, &fixture.blobs, 8 * 1024, bytes).unwrap();

    assert_eq!(spilled.len(), 1);
    assert_eq!(spilled[0].field, "arguments/contents");
    assert_eq!(spilled[0].bytes, output.len() as u64);
    assert!(bytes(&line).unwrap() <= 8 * 1024, "the line fits");

    let kept = line["arguments"]["contents"].as_str().unwrap();
    assert!(kept.starts_with('x'), "the preview identifies the payload");
    assert!(
      kept.contains(&format!("stored {} bytes", output.len())),
      "the line says how much it left out: {kept}"
    );
    assert!(
      kept.contains(&format!(", {} bytes shown]", PREVIEW_BYTES)),
      "and how much it kept: {kept}"
    );

    // The bytes are recoverable exactly, by the reference the line names.
    let blob = BlobRef::for_bytes(output.as_bytes(), None);
    assert_eq!(spilled[0].reference, blob.relative_path());
    assert_eq!(fixture.blobs.get(&blob).unwrap(), output.as_bytes());
    assert!(fixture.blobs.verify(&blob).unwrap());
  }

  #[test]
  fn identical_payloads_are_stored_once() {
    let fixture = fixture().unwrap();
    let output = "same bytes".repeat(2 * 1024);
    let mut first = json!({"type": "tool_completed", "output": output});
    let mut second = json!({"type": "tool_completed", "output": output});
    bound(&mut first, &fixture.blobs, 1024, bytes).unwrap();
    bound(&mut second, &fixture.blobs, 1024, bytes).unwrap();
    assert_eq!(
      fixture.blobs.bytes().unwrap(),
      output.len() as u64,
      "content addressing is the deduplication"
    );
  }

  #[test]
  fn the_largest_field_is_spilled_before_a_smaller_one() {
    let fixture = fixture().unwrap();
    let mut line = json!({
      "type": "tool_requested",
      "arguments": {"contents": "b".repeat(30 * 1024), "path": "note.txt"},
      "name": "write",
    });
    let spilled = bound(&mut line, &fixture.blobs, 4 * 1024, bytes).unwrap();
    assert_eq!(spilled.len(), 1);
    assert_eq!(spilled[0].field, "arguments/contents");
    assert_eq!(line["arguments"]["path"], "note.txt", "untouched");
    assert_eq!(line["name"], "write");
  }

  #[test]
  fn spilling_continues_until_the_line_fits() {
    let fixture = fixture().unwrap();
    let mut line = json!({
      "type": "tool_completed",
      "message": "a".repeat(20 * 1024),
      "status_text": "b".repeat(20 * 1024),
    });
    let spilled = bound(&mut line, &fixture.blobs, 4 * 1024, bytes).unwrap();
    assert_eq!(spilled.len(), 2, "both payloads came out");
    assert!(bytes(&line).unwrap() <= 4 * 1024, "the line fits");
    let fields: Vec<&str> = spilled.iter().map(|field| field.field.as_str()).collect();
    assert_eq!(fields, ["message", "status_text"], "ties break on the path");
  }

  #[test]
  fn a_budget_that_cannot_be_met_leaves_the_line_usable() {
    let fixture = fixture().unwrap();
    // No budget can hold this envelope plus a marker: the identity fields, which
    // must survive, are already longer than the budget.
    let mut line =
      json!({"type": "tool_completed", "call_id": "call-very-long", "message": "z".repeat(900)});
    let spilled = bound(&mut line, &fixture.blobs, 8, bytes).unwrap();
    assert_eq!(
      spilled.len(),
      1,
      "the only thing that could be dropped was dropped"
    );
    assert_eq!(line["type"], "tool_completed", "the line still decodes");
    assert_eq!(line["call_id"], "call-very-long");
  }

  #[test]
  fn pointers_and_identity_are_never_elided() {
    let fixture = fixture().unwrap();
    let long = "q".repeat(30 * 1024);
    let mut line = json!({
      "type": "context_reduced",
      "recovery_ref": long,
      "trace_id": long,
      "meta": {"event_id": long, "seq": 4},
      "message": long,
    });
    let spilled = bound(&mut line, &fixture.blobs, 4 * 1024, bytes).unwrap();
    assert_eq!(
      spilled
        .iter()
        .map(|field| field.field.as_str())
        .collect::<Vec<_>>(),
      ["message"],
      "a pointer that cannot be followed is worse than a long line"
    );
    assert_eq!(line["recovery_ref"], long);
    assert_eq!(line["trace_id"], long);
    assert_eq!(line["meta"]["event_id"], long);
  }

  #[test]
  fn a_line_of_many_medium_fields_stops_bounding_after_its_share_of_work() {
    let fixture = fixture().unwrap();
    // 200 fields of 400 bytes: no single spill gets close to a 1 KiB budget, so the
    // search would otherwise keep going while writing almost nothing.
    let mut line = json!({"type": "tool_requested", "arguments": {}});
    for index in 0..200 {
      line["arguments"][format!("hunk_{index}")] = json!("h".repeat(400));
    }
    let before = bytes(&line).unwrap();
    let spilled = bound(&mut line, &fixture.blobs, 1024, bytes).unwrap();
    assert_eq!(
      spilled.len(),
      MAX_SPILLS_PER_LINE,
      "bounded work, not unbounded"
    );
    let after = bytes(&line).unwrap();
    assert!(after < before, "and it still made real progress");
    assert!(
      serde_json::to_string(&line).is_ok(),
      "the line stays a line even though the budget is unmet"
    );
  }

  #[test]
  fn a_pointer_record_is_not_a_candidate_even_when_it_is_the_largest_string() {
    let fixture = fixture().unwrap();
    // Tool outputs are externalized by the tool layer, so the blob record that
    // points at one is the longest string on the line. Spilling it would remove
    // the only way back to the bytes it describes.
    let blob = BlobRef::for_bytes(b"tool output", None);
    let mut line = json!({
      "type": "tool_completed",
      "call_id": "call_1",
      "name": "read",
      "visible_bytes": 11,
      "blob": {
        "hash": blob.hash,
        "relative_path": blob.relative_path(),
        "media_type": "text/plain",
      },
    });
    let before = line.clone();
    let spilled = bound(&mut line, &fixture.blobs, 8, bytes).unwrap();
    assert!(spilled.is_empty(), "nothing on this line may be spilled");
    assert_eq!(line, before);
    assert_eq!(fixture.blobs.bytes().unwrap(), 0);
  }

  #[test]
  fn a_preview_never_splits_a_character() {
    let fixture = fixture().unwrap();
    // 256 bytes lands in the middle of a three-byte character.
    let mut text = "a".repeat(PREVIEW_BYTES - 1);
    text.push('⇄');
    text.push_str(&"a".repeat(4 * 1024));
    let mut line = json!({"type": "tool_completed", "message": text});
    let spilled = bound(&mut line, &fixture.blobs, 1024, bytes).unwrap();
    assert_eq!(spilled.len(), 1);
    let kept = line["message"].as_str().unwrap();
    assert!(std::str::from_utf8(kept.as_bytes()).is_ok());
    assert!(kept.starts_with(&text[..PREVIEW_BYTES - 1]));
    assert!(
      !kept.contains('\u{fffd}'),
      "no replacement character invented"
    );
  }

  #[test]
  fn a_value_shorter_than_its_marker_stays_inline() {
    let fixture = fixture().unwrap();
    // The marker names a 64-hex hash, so it is long; a 300-byte value would grow.
    let mut line = json!({"type": "tool_completed", "message": "y".repeat(300)});
    let spilled = bound(&mut line, &fixture.blobs, 1, bytes).unwrap();
    assert!(spilled.is_empty(), "spilling cost more than it saved");
    assert_eq!(line["message"].as_str().unwrap().len(), 300);
  }
}
