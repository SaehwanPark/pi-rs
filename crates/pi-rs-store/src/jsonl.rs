//! Append-only JSONL primitives shared by the session log and the trace
//! journal.
//!
//! Two properties are load-bearing here:
//!
//! - **Tolerant reading.** A journal is written while the process may be
//!   killed, so a truncated final line is normal rather than corrupt. Skipping
//!   unusable lines, and *counting* them, keeps a session openable while still
//!   reporting that something was lost.
//! - **Buffered, classified writing.** Streaming deltas must not hit the disk
//!   one line at a time, and important transitions must not be sitting in a
//!   buffer when the process dies. The caller classifies each line as durable
//!   or not; this module only implements the buffering.

use std::{
  fs::{self, File, OpenOptions},
  io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
  path::{Path, PathBuf},
};

use serde::de::DeserializeOwned;

use crate::StoreError;

/// How many decoded and how many unusable lines a read produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadReport<T> {
  pub items: Vec<T>,
  pub malformed: usize,
  /// Line number of the first unusable line, within the decoded region.
  ///
  /// A count alone says the file is damaged; the number says where to look.
  /// Tolerating a bad line without locating it turns a diagnosable incident
  /// into a mysterious gap in history.
  pub first_malformed_line: Option<usize>,
}

impl<T> ReadReport<T> {
  pub fn len(&self) -> usize {
    self.items.len()
  }

  pub fn is_empty(&self) -> bool {
    self.items.is_empty()
  }
}

/// Maximum line size accepted by the general JSONL readers.
///
/// Session messages and trace payloads may be large, but an unbounded `read_line`
/// turns a corrupt file into an allocation primitive. Oversized lines are counted
/// as malformed and therefore fail closed at the store validation boundary.
pub const MAX_JSONL_LINE_BYTES: usize = 16 * 1024 * 1024;

/// Decode one JSONL file, skipping unusable lines.
pub fn read_jsonl<T: DeserializeOwned>(path: &Path) -> Result<ReadReport<T>, StoreError> {
  read_jsonl_with_limit(path, MAX_JSONL_LINE_BYTES)
}

/// Decode one JSONL file with an explicit per-line bound.
pub(crate) fn read_jsonl_with_limit<T: DeserializeOwned>(
  path: &Path,
  max_line_bytes: usize,
) -> Result<ReadReport<T>, StoreError> {
  read_jsonl_with_limits(path, max_line_bytes, usize::MAX)
}

/// Decode one JSONL file with both per-line and decoded-entry bounds.
///
/// The entry bound matters for small, valid lines: a file made of millions of
/// tiny records can exhaust memory even when every individual line is safe.
pub(crate) fn read_jsonl_with_limits<T: DeserializeOwned>(
  path: &Path,
  max_line_bytes: usize,
  max_items: usize,
) -> Result<ReadReport<T>, StoreError> {
  let file = open(path)?;
  let mut reader = BufReader::new(file);
  let mut items = Vec::new();
  let mut malformed = 0usize;
  let mut first_malformed_line = None;
  let mut number = 0usize;
  while let Some((line, oversized)) = read_bounded_line(&mut reader, max_line_bytes)? {
    number += 1;
    if oversized {
      malformed += 1;
      first_malformed_line.get_or_insert(number);
      continue;
    }
    let trimmed = line.strip_suffix(b"\n").unwrap_or(&line);
    let trimmed = trimmed.strip_suffix(b"\r").unwrap_or(trimmed);
    if trimmed.iter().all(u8::is_ascii_whitespace) {
      continue;
    }
    match serde_json::from_slice(trimmed) {
      Ok(item) => {
        if items.len() >= max_items {
          return Err(StoreError::Invalid(format!(
            "{} contains more than {max_items} decoded JSONL entries",
            path.display()
          )));
        }
        items.push(item);
      }
      Err(_) => {
        malformed += 1;
        first_malformed_line.get_or_insert(number);
      }
    }
  }
  Ok(ReadReport {
    items,
    malformed,
    first_malformed_line,
  })
}

/// Read and discard one bounded line without allocating its unbounded suffix.
fn read_bounded_line<R: BufRead>(
  reader: &mut R,
  max_line_bytes: usize,
) -> std::io::Result<Option<(Vec<u8>, bool)>> {
  let mut line = Vec::new();
  let mut oversized = false;
  loop {
    let available = reader.fill_buf()?;
    if available.is_empty() {
      if line.is_empty() && !oversized {
        return Ok(None);
      }
      return Ok(Some((line, oversized)));
    }
    let end = available
      .iter()
      .position(|byte| *byte == b'\n')
      .map_or(available.len(), |index| index + 1);
    if !oversized {
      if line.len().saturating_add(end) > max_line_bytes {
        oversized = true;
        line.clear();
      } else {
        line.extend_from_slice(&available[..end]);
      }
    }
    let has_newline = available.get(end.saturating_sub(1)) == Some(&b'\n');
    reader.consume(end);
    if has_newline {
      return Ok(Some((line, oversized)));
    }
  }
}

/// Decode the tail of a JSONL file without reading the whole thing.
///
/// Used to recover the last sequence number when a session is reopened, which
/// must not require hydrating a long trace. The first line inside the window is
/// dropped when it starts mid-line, and a window this small cannot hide a
/// truncation that matters for sequence recovery.
pub fn read_jsonl_tail<T: DeserializeOwned>(
  path: &Path,
  max_bytes: u64,
) -> Result<ReadReport<T>, StoreError> {
  let mut file = open(path)?;
  let size = file.metadata()?.len();
  if size == 0 {
    return Ok(ReadReport {
      items: Vec::new(),
      malformed: 0,
      first_malformed_line: None,
    });
  }
  let window = size.min(max_bytes.max(1));
  let skip = size - window;
  if skip > 0 {
    file.seek(SeekFrom::Start(skip))?;
  }
  let mut bytes = Vec::with_capacity(window as usize);
  file.read_to_end(&mut bytes)?;
  let text = String::from_utf8_lossy(&bytes).into_owned();
  let mut lines: Vec<&str> = text.lines().collect();
  if skip > 0 {
    // The window almost certainly began inside a line. Only drop it when it is
    // actually a fragment: a window that happens to start at a boundary must
    // keep its first line.
    let starts_mid_line = !text.starts_with('\n') && !is_complete_json(lines.first().copied());
    if starts_mid_line {
      lines.remove(0);
    }
  }
  let mut items = Vec::new();
  let mut malformed = 0usize;
  let mut first_malformed_line = None;
  for (index, line) in lines.iter().enumerate() {
    let trimmed = line.trim();
    if trimmed.is_empty() {
      continue;
    }
    match serde_json::from_str(trimmed) {
      Ok(item) => items.push(item),
      Err(_) => {
        malformed += 1;
        first_malformed_line.get_or_insert(index + 1);
      }
    }
  }
  Ok(ReadReport {
    items,
    malformed,
    first_malformed_line,
  })
}

fn is_complete_json(line: Option<&str>) -> bool {
  let Some(line) = line else { return false };
  serde_json::from_str::<serde_json::Value>(line.trim()).is_ok()
}

/// Read the first line only.
///
/// Session metadata lives in the first line, which is what makes `pi-rs
/// sessions` cheap: listing reads one line per session instead of the bodies.
pub fn read_first_line(path: &Path) -> Result<Option<String>, StoreError> {
  let file = match open(path) {
    Ok(file) => file,
    // Absent and empty mean the same thing to a caller that is deciding whether
    // a header exists: it does not. Returning an error here would push callers
    // into `exists()` checks that race the writer.
    Err(StoreError::Missing(_)) => return Ok(None),
    Err(error) => return Err(error),
  };
  let mut reader = BufReader::new(file);
  let Some((line, oversized)) = read_bounded_line(&mut reader, MAX_JSONL_LINE_BYTES)? else {
    return Ok(None);
  };
  if oversized {
    return Err(StoreError::Invalid(format!(
      "{} has a session header larger than the {}-byte JSONL limit",
      path.display(),
      MAX_JSONL_LINE_BYTES
    )));
  }
  let trimmed = String::from_utf8(line)
    .map_err(|error| {
      StoreError::Invalid(format!(
        "{} has an invalid UTF-8 header: {error}",
        path.display()
      ))
    })?
    .trim_end_matches(['\n', '\r'])
    .to_string();
  if trimmed.is_empty() {
    return Ok(None);
  }
  Ok(Some(trimmed))
}

fn open(path: &Path) -> Result<File, StoreError> {
  match File::open(path) {
    Ok(file) => Ok(file),
    Err(error) if StoreError::is_missing(&error) => {
      Err(StoreError::Missing(path.display().to_string()))
    }
    Err(error) => Err(StoreError::Io(error)),
  }
}

/// Buffered append-only writer.
///
/// The writer deliberately does not buffer when a caller marks a line durable,
/// and it never keeps more than [`LineWriter::MAX_BUFFERED_BYTES`] pending, so
/// buffering is a latency optimization with a bounded exposure rather than a
/// data-loss policy.
#[derive(Debug)]
pub struct LineWriter {
  path: PathBuf,
  file: File,
  buffer: String,
  pending: usize,
  buffered_bytes: usize,
}

impl LineWriter {
  pub const MAX_BUFFERED_BYTES: usize = 64 * 1024;

  /// Open for append, creating the file and its parent directory if needed.
  ///
  /// `write(true)` is explicitly paired with `append(true)` so Windows file
  /// handles receive `FILE_GENERIC_WRITE` rather than `FILE_APPEND_DATA` only,
  /// allowing `clear` to truncate via `set_len(0)`.
  #[allow(clippy::ineffective_open_options)]
  pub fn create(path: &Path) -> Result<Self, StoreError> {
    if let Some(parent) = path.parent() {
      fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
      .write(true)
      .append(true)
      .create(true)
      .open(path)?;
    Ok(Self {
      path: path.to_path_buf(),
      file,
      buffer: String::new(),
      pending: 0,
      buffered_bytes: 0,
    })
  }

  pub fn path(&self) -> &Path {
    &self.path
  }

  /// Lines accepted since the last flush.
  pub fn pending(&self) -> usize {
    self.pending
  }

  /// Write one already-encoded line. A `durable` line is written and flushed
  /// immediately.
  pub fn write_line(&mut self, line: &str, durable: bool) -> Result<(), StoreError> {
    self.buffer.push_str(line);
    self.buffer.push('\n');
    self.pending += 1;
    self.buffered_bytes += line.len() + 1;
    if durable || self.buffered_bytes >= Self::MAX_BUFFERED_BYTES {
      self.flush()?;
    }
    Ok(())
  }

  pub fn flush(&mut self) -> Result<(), StoreError> {
    if self.buffer.is_empty() {
      return Ok(());
    }
    self.file.write_all(self.buffer.as_bytes())?;
    self.file.flush()?;
    // A durable transition must survive a process crash, not merely reach the
    // kernel's page cache. Streaming deltas still amortize this at the bounded
    // buffer threshold; semantic lines pay the sync cost deliberately.
    self.file.sync_data()?;
    self.buffer.clear();
    self.pending = 0;
    self.buffered_bytes = 0;
    Ok(())
  }

  /// Truncate a journal that has no pending semantic work.
  ///
  /// This is used by the portable WAL compactor on platforms where replacing
  /// an open file is not reliable. Callers must establish that no intent is
  /// pending before clearing; an interrupted clear can only lose already
  /// committed WAL history, never an uncommitted projection.
  pub fn clear(&mut self) -> Result<(), StoreError> {
    self.flush()?;
    self.file.set_len(0)?;
    self.file.seek(SeekFrom::Start(0))?;
    self.file.sync_data()?;
    self.buffer.clear();
    self.pending = 0;
    self.buffered_bytes = 0;
    Ok(())
  }
}

impl Drop for LineWriter {
  fn drop(&mut self) {
    // Best-effort: dropping must not panic, and an unwritable buffer here means
    // the process is already losing state the caller will be told about.
    let _ = self.flush();
  }
}

#[cfg(test)]
mod tests {
  use serde::{Deserialize, Serialize};

  use crate::tmp::TempDir;

  use super::*;

  #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
  struct Line {
    seq: u64,
    text: String,
  }

  fn path(tmp: &TempDir, name: &str) -> PathBuf {
    tmp.path().join(name)
  }

  #[test]
  fn buffering_is_visible_and_flush_is_not_lossy() {
    let tmp = TempDir::new("jsonl-buffer");
    let target = path(&tmp, "journal.jsonl");
    let mut writer = LineWriter::create(&target).unwrap();
    writer.write_line(&line_json(1, "a"), false).unwrap();
    writer.write_line(&line_json(2, "b"), false).unwrap();
    assert_eq!(writer.pending(), 2);
    assert_eq!(
      fs::read_to_string(&target).unwrap(),
      "",
      "buffered lines stay in memory"
    );
    writer.flush().unwrap();
    assert_eq!(writer.pending(), 0);
    assert_eq!(fs::read_to_string(&target).unwrap().lines().count(), 2);
  }

  #[test]
  fn durable_lines_bypass_the_buffer() {
    let tmp = TempDir::new("jsonl-durable");
    let target = path(&tmp, "journal.jsonl");
    let mut writer = LineWriter::create(&target).unwrap();
    writer.write_line(&line_json(1, "delta"), false).unwrap();
    writer
      .write_line(&line_json(2, "tool completed"), true)
      .unwrap();
    let on_disk = fs::read_to_string(&target).unwrap();
    assert_eq!(
      on_disk.lines().count(),
      2,
      "durable write must drain the buffer"
    );
    assert!(on_disk.contains("tool completed"));
  }

  #[test]
  fn buffer_is_bounded_even_without_durable_lines() {
    let tmp = TempDir::new("jsonl-bound");
    let target = path(&tmp, "journal.jsonl");
    let mut writer = LineWriter::create(&target).unwrap();
    let big = "x".repeat(4 * 1024);
    for seq in 0..40 {
      writer.write_line(&line_json(seq, &big), false).unwrap();
    }
    assert!(
      writer.pending() <= 16,
      "buffer must drain at the byte bound, pending {}",
      writer.pending()
    );
  }

  #[test]
  fn drop_flushes_pending_lines() {
    let tmp = TempDir::new("jsonl-drop");
    let target = path(&tmp, "journal.jsonl");
    {
      let mut writer = LineWriter::create(&target).unwrap();
      writer.write_line(&line_json(1, "bye"), false).unwrap();
    }
    assert_eq!(fs::read_to_string(&target).unwrap().lines().count(), 1);
  }

  #[test]
  fn truncated_and_garbage_lines_are_skipped_and_counted() {
    let tmp = TempDir::new("jsonl-corrupt");
    let target = path(&tmp, "journal.jsonl");
    fs::write(
      &target,
      format!(
        "{}\nnot json\n{}\n{}\n",
        line_json(1, "a"),
        line_json(3, "c"),
        r#"{"seq":4,"text":"trunc"#
      ),
    )
    .unwrap();
    let report: ReadReport<Line> = read_jsonl(&target).unwrap();
    assert_eq!(report.items.len(), 2);
    assert_eq!(
      report.malformed, 2,
      "one garbage line and one truncated line"
    );
    assert_eq!(report.items[1].seq, 3);
  }

  #[test]
  fn tail_read_recovers_the_end_without_the_whole_file() {
    let tmp = TempDir::new("jsonl-tail");
    let target = path(&tmp, "journal.jsonl");
    let mut writer = LineWriter::create(&target).unwrap();
    for seq in 0..2_000 {
      writer
        .write_line(&line_json(seq, &format!("delta {seq}")), true)
        .unwrap();
    }
    writer.flush().unwrap();
    let size = fs::metadata(&target).unwrap().len();
    let tail: ReadReport<Line> = read_jsonl_tail(&target, 4 * 1024).unwrap();
    assert!(size > 4 * 1024);
    assert!(
      tail.items.len() < 2_000 && tail.items.len() > 10,
      "tail must be a bounded suffix, got {}",
      tail.items.len()
    );
    assert_eq!(tail.items.last().unwrap().seq, 1_999);
    assert_eq!(
      tail.malformed, 0,
      "a mid-line window must not count as damage"
    );
    // A window larger than the file keeps every line.
    let all: ReadReport<Line> = read_jsonl_tail(&target, size + 10).unwrap();
    assert_eq!(all.items.len(), 2_000);
  }

  #[test]
  fn first_line_read_is_cheap_and_reports_absence() {
    let tmp = TempDir::new("jsonl-first");
    let target = path(&tmp, "journal.jsonl");
    assert!(matches!(read_first_line(&target), Ok(None)));
    fs::write(&target, "first\nsecond\n").unwrap();
    assert_eq!(read_first_line(&target).unwrap().as_deref(), Some("first"));
    fs::write(&target, "").unwrap();
    assert_eq!(read_first_line(&target).unwrap(), None);
  }

  #[test]
  fn clear_truncates_the_file_and_allows_subsequent_writes() {
    let tmp = TempDir::new("jsonl-clear");
    let target = path(&tmp, "journal.jsonl");
    let mut writer = LineWriter::create(&target).unwrap();
    writer.write_line(&line_json(1, "first"), true).unwrap();
    writer.write_line(&line_json(2, "second"), true).unwrap();
    assert_eq!(fs::read_to_string(&target).unwrap().lines().count(), 2);

    writer.clear().unwrap();
    assert_eq!(
      fs::read_to_string(&target).unwrap(),
      "",
      "cleared file is empty"
    );

    writer.write_line(&line_json(3, "third"), true).unwrap();
    let content = fs::read_to_string(&target).unwrap();
    assert_eq!(
      content.lines().count(),
      1,
      "only the line written after clear is present"
    );
    assert!(content.contains("third"));
  }

  fn line_json(seq: u64, text: &str) -> String {
    serde_json::to_string(&Line {
      seq,
      text: text.to_string(),
    })
    .unwrap()
  }
}
