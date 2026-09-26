//! Server-sent-events framing for OpenAI-compatible streams.
//!
//! This is deliberately *not* a general SSE implementation. It understands the
//! subset that OpenAI-compatible chat-completion endpoints emit: `data:` lines,
//! one JSON object per event, terminated by `data: [DONE]`. Keeping it small is
//! the point — the alternative, hand-parsing framing inside the adapter, is
//! where stream-state bugs usually hide.

use std::io::{self, BufReader, Read};

use rupi_core::{BoundedLineReader, LineOverflow};

/// Largest accepted SSE frame in bytes, including ignored metadata and comments.
///
/// A confused or hostile endpoint that never emits a blank line must not be
/// able to grow the buffer without bound. One chat-completion frame is a few
/// hundred bytes; a whole megabyte is already far outside that.
pub const MAX_EVENT_BYTES: usize = 1024 * 1024;

/// Maximum complete SSE frames accepted for one streamed response.
///
/// Semantic event limits do not count empty-choice or usage-only frames, but
/// they still consume parsing work and reset the transport idle timer.
pub const MAX_RESPONSE_FRAMES: usize = 65_536;

/// The termination sentinel used by OpenAI-compatible endpoints.
pub const DONE: &str = "[DONE]";

/// One dispatched event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
  /// Concatenated `data:` payload.
  pub data: String,
}

impl SseEvent {
  /// `true` when this event terminates the stream.
  pub fn is_done(&self) -> bool {
    self.data.trim() == DONE
  }
}

/// Incremental event reader over a byte stream.
pub struct SseStream<R> {
  reader: BufReader<R>,
  lines: BoundedLineReader,
  pending_data: String,
  pending_received: bool,
  pending_frame_bytes: usize,
  response_frames: usize,
  finished: bool,
}

impl<R: Read> SseStream<R> {
  pub fn new(reader: R) -> Self {
    Self {
      reader: BufReader::with_capacity(8 * 1024, reader),
      lines: BoundedLineReader::new(),
      pending_data: String::new(),
      pending_received: false,
      pending_frame_bytes: 0,
      response_frames: 0,
      finished: false,
    }
  }

  fn finish_frame(&mut self) -> io::Result<()> {
    if self.response_frames >= MAX_RESPONSE_FRAMES {
      self.finished = true;
      return Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("sse response exceeds {MAX_RESPONSE_FRAMES} frames"),
      ));
    }
    self.response_frames += 1;
    self.pending_frame_bytes = 0;
    Ok(())
  }

  /// Next event, or `None` at end of stream.
  ///
  /// A server that closes the connection without `[DONE]` is not an error here:
  /// the adapter decides whether an unterminated stream is a truncation,
  /// because only it knows whether a finish reason already arrived.
  pub fn next_event(&mut self) -> io::Result<Option<SseEvent>> {
    if self.finished {
      return Ok(None);
    }
    let mut data = std::mem::take(&mut self.pending_data);
    let mut received = self.pending_received;
    self.pending_received = false;
    loop {
      let line = match self
        .lines
        .read_line(&mut self.reader, MAX_EVENT_BYTES, LineOverflow::Reject)
      {
        Ok(Some(line)) => line,
        Ok(None) => {
          // EOF. A trailing event without a blank line is still a real event.
          self.finished = true;
          self.pending_data.clear();
          self.pending_received = false;
          return if received {
            self.finish_frame()?;
            Ok(Some(SseEvent { data }))
          } else {
            Ok(None)
          };
        }
        Err(error) => {
          // A socket read timeout is transient. Keep the event accumulated so
          // far in the stream and let the adapter check cancellation before it
          // retries. Other errors still close the protocol.
          self.pending_data = data;
          self.pending_received = received;
          return Err(error);
        }
      };
      let raw_line = line.into_bytes();
      self.pending_frame_bytes = self.pending_frame_bytes.saturating_add(raw_line.len());
      if self.pending_frame_bytes > MAX_EVENT_BYTES {
        self.finished = true;
        return Err(io::Error::new(
          io::ErrorKind::InvalidData,
          format!("sse frame exceeds {MAX_EVENT_BYTES} bytes"),
        ));
      }
      let line = String::from_utf8(raw_line).map_err(|error| {
        self.finished = true;
        io::Error::new(io::ErrorKind::InvalidData, error)
      })?;
      let trimmed = line.trim_end_matches(['\n', '\r']);
      if trimmed.is_empty() {
        self.finish_frame()?;
        self.finished |= received && data.trim() == DONE;
        self.pending_data.clear();
        self.pending_received = false;
        return if received {
          Ok(Some(SseEvent { data }))
        } else {
          // Successive blank lines carry no information; keep reading.
          continue;
        };
      }
      if trimmed.starts_with(':') {
        // Comment / keep-alive.
        continue;
      }
      let Some((field, value)) = trimmed.split_once(':') else {
        continue;
      };
      let value = value.strip_prefix(' ').unwrap_or(value);
      if field == "data" {
        {
          if data.len() + value.len() > MAX_EVENT_BYTES {
            self.finished = true;
            return Err(io::Error::new(
              io::ErrorKind::InvalidData,
              format!("sse event exceeds {MAX_EVENT_BYTES} bytes"),
            ));
          }
          if received {
            data.push('\n');
          }
          data.push_str(value);
          received = true;
          self.pending_received = received;
        }
      }
      // `event:`, `id:`, and `retry:` are irrelevant for chat completions, and
      // `event:` in particular is never sent by these endpoints.
    }
  }

  /// Underlying reader, for callers that need the raw remainder.
  pub fn into_inner(self) -> R {
    self.reader.into_inner()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn events(input: &str) -> Vec<SseEvent> {
    let mut stream = SseStream::new(input.as_bytes());
    let mut out = Vec::new();
    while let Some(event) = stream.next_event().unwrap() {
      let done = event.is_done();
      out.push(event);
      if done {
        break;
      }
    }
    out
  }

  #[test]
  fn data_lines_become_events() {
    let got = events("data: {\"a\":1}\n\ndata: {\"a\":2}\n\ndata: [DONE]\n\n");
    assert_eq!(got.len(), 3);
    assert_eq!(got[0].data, "{\"a\":1}");
    assert!(got[2].is_done());
  }

  #[test]
  fn crlf_and_keepalives_are_tolerated() {
    let got = events(": ping\r\n\r\ndata: {\"a\":1}\r\n\r\n");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].data, "{\"a\":1}");
  }

  #[test]
  fn multi_line_data_joins_with_newline() {
    let got = events("data: {\"a\":\ndata: 1}\n\n");
    assert_eq!(got[0].data, "{\"a\":\n1}");
  }

  #[test]
  fn eof_without_done_is_not_an_error() {
    let got = events("data: {\"a\":1}\n\n");
    assert_eq!(got.len(), 1);
    let mut stream = SseStream::new("data: {\"a\":1}".as_bytes());
    assert!(stream.next_event().unwrap().is_some());
    assert!(stream.next_event().unwrap().is_none(), "stream is drained");
  }

  #[test]
  fn an_endpoint_that_never_blanks_the_line_cannot_grow_the_buffer() {
    let line = format!("data: {}", "x".repeat(MAX_EVENT_BYTES + 2048));
    let error = SseStream::new(line.as_bytes())
      .next_event()
      .expect_err("oversized line must fail, not allocate");
    assert!(error.to_string().contains("exceeds"), "{error}");
  }

  #[test]
  fn an_event_with_many_data_lines_is_bounded_as_a_whole() {
    let line = format!("data: {}\n", "x".repeat(2048));
    let flood = line.repeat(1_000);
    let error = SseStream::new(flood.as_bytes())
      .next_event()
      .expect_err("oversized event must fail, not allocate");
    assert!(error.to_string().contains("exceeds"), "{error}");
  }
}
