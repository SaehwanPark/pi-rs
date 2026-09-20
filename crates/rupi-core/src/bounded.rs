//! Bounded ingestion for line-oriented boundaries.
//!
//! `BufRead::read_line` grows its destination until it sees a newline. That is
//! convenient for trusted text, but unsafe at provider and subprocess protocol
//! boundaries where the peer controls framing. This helper inspects the reader's
//! existing buffer first and never copies more than the caller's byte ceiling.

use std::io::{self, BufRead};

/// What to do when a line exceeds the supplied byte limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineOverflow {
  /// Return `InvalidData` immediately. The caller should close the protocol.
  Reject,
  /// Keep a bounded prefix, consume through the newline, and report truncation.
  Truncate,
}

/// One bounded line, excluding no framing bytes except that an absent final
/// newline remains absent. A complete line normally includes its `\\n`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundedLine {
  Complete(Vec<u8>),
  Truncated(Vec<u8>),
}

impl BoundedLine {
  /// Bytes retained from the line, including a newline when it fit and existed.
  pub fn as_bytes(&self) -> &[u8] {
    match self {
      Self::Complete(bytes) | Self::Truncated(bytes) => bytes,
    }
  }

  /// Whether bytes were discarded after the retention ceiling was reached.
  pub fn is_truncated(&self) -> bool {
    matches!(self, Self::Truncated(_))
  }

  pub fn into_bytes(self) -> Vec<u8> {
    match self {
      Self::Complete(bytes) | Self::Truncated(bytes) => bytes,
    }
  }
}

/// Stateful bounded line reader.
///
/// The state is important for transports whose underlying socket can return a
/// transient timeout: already-consumed bytes remain owned by this reader and
/// are resumed on the next call instead of being silently dropped.
#[derive(Debug, Default)]
pub struct BoundedLineReader {
  line: Vec<u8>,
  truncated: bool,
}

impl BoundedLineReader {
  pub fn new() -> Self {
    Self::default()
  }

  /// Read one line without materializing an unbounded peer-controlled value.
  ///
  /// The limit applies to line content and not the terminating newline. With
  /// [`LineOverflow::Reject`], no bytes beyond the bounded prefix are consumed
  /// on overflow; protocol callers should close the transport. With
  /// [`LineOverflow::Truncate`], the remainder is consumed without being
  /// retained so a local text scan can continue to the next line.
  pub fn read_line<R: BufRead>(
    &mut self,
    reader: &mut R,
    limit: usize,
    overflow: LineOverflow,
  ) -> io::Result<Option<BoundedLine>> {
    if limit == 0 {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "bounded line limit must be greater than zero",
      ));
    }
    if self.line.is_empty() && !self.truncated {
      self.line.reserve(limit.min(8 * 1024));
    }

    loop {
      // Preserve the partially accumulated line across transient socket
      // errors. The caller may retry after checking cancellation.
      let buffer = reader.fill_buf()?;
      if buffer.is_empty() {
        return if self.line.is_empty() {
          Ok(None)
        } else {
          Ok(Some(self.take_line()))
        };
      }

      let newline = buffer.iter().position(|byte| *byte == b'\n');
      let content_len = newline.unwrap_or(buffer.len());

      if !self.truncated {
        let remaining = limit.saturating_sub(self.line.len());
        if content_len <= remaining {
          self.line.extend_from_slice(&buffer[..content_len]);
          if let Some(index) = newline {
            // The newline is framing, not content, and is retained for callers
            // that already trim line endings themselves.
            self.line.push(b'\n');
            reader.consume(index + 1);
            return Ok(Some(self.take_line()));
          }
          reader.consume(content_len);
          continue;
        }

        match overflow {
          LineOverflow::Reject => {
            return Err(io::Error::new(
              io::ErrorKind::InvalidData,
              format!("line exceeds {limit} bytes"),
            ));
          }
          LineOverflow::Truncate => {
            if remaining > 0 {
              self.line.extend_from_slice(&buffer[..remaining]);
            }
            self.truncated = true;
          }
        }
      }

      // Truncating callers must consume the rest of this line, but never retain
      // it. Rejecting callers returned above before consuming any overflow.
      let take = newline.map_or(buffer.len(), |index| index + 1);
      reader.consume(take);
      if newline.is_some() {
        return Ok(Some(self.take_line()));
      }
    }
  }

  fn take_line(&mut self) -> BoundedLine {
    let bytes = std::mem::take(&mut self.line);
    let truncated = std::mem::take(&mut self.truncated);
    if truncated {
      BoundedLine::Truncated(bytes)
    } else {
      BoundedLine::Complete(bytes)
    }
  }
}

/// Read one line without materializing an unbounded peer-controlled value.
///
/// This convenience wrapper is suitable when the caller does not need to
/// resume a partially read line after a transient I/O error.
pub fn read_bounded_line<R: BufRead>(
  reader: &mut R,
  limit: usize,
  overflow: LineOverflow,
) -> io::Result<Option<BoundedLine>> {
  BoundedLineReader::new().read_line(reader, limit, overflow)
}

#[cfg(test)]
mod tests {
  use std::io::Cursor;

  use super::*;

  #[test]
  fn complete_lines_keep_the_newline() {
    let mut reader = Cursor::new(b"one\ntwo".to_vec());
    assert_eq!(
      read_bounded_line(&mut reader, 8, LineOverflow::Reject).unwrap(),
      Some(BoundedLine::Complete(b"one\n".to_vec()))
    );
    assert_eq!(
      read_bounded_line(&mut reader, 8, LineOverflow::Reject).unwrap(),
      Some(BoundedLine::Complete(b"two".to_vec()))
    );
    assert_eq!(
      read_bounded_line(&mut reader, 8, LineOverflow::Reject).unwrap(),
      None
    );
  }

  #[test]
  fn rejection_does_not_allocate_the_whole_line() {
    let mut reader = Cursor::new(format!("{}\n", "x".repeat(1_000_000)).into_bytes());
    let error = read_bounded_line(&mut reader, 32, LineOverflow::Reject).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
  }

  #[test]
  fn truncation_consumes_the_remainder_and_continues() {
    let mut reader = Cursor::new(b"abcdef\nok\n".to_vec());
    assert_eq!(
      read_bounded_line(&mut reader, 3, LineOverflow::Truncate).unwrap(),
      Some(BoundedLine::Truncated(b"abc".to_vec()))
    );
    assert_eq!(
      read_bounded_line(&mut reader, 3, LineOverflow::Truncate).unwrap(),
      Some(BoundedLine::Complete(b"ok\n".to_vec()))
    );
  }
}
