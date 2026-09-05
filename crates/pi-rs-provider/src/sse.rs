//! Server-sent-events framing for OpenAI-compatible streams.
//!
//! This is deliberately *not* a general SSE implementation. It understands the
//! subset that OpenAI-compatible chat-completion endpoints emit: `data:` lines,
//! one JSON object per event, terminated by `data: [DONE]`. Keeping it small is
//! the point — the alternative, hand-parsing framing inside the adapter, is
//! where stream-state bugs usually hide.

use std::io::{self, BufRead, BufReader, Read};

/// Largest accepted event payload.
///
/// A confused or hostile endpoint that never emits a blank line must not be
/// able to grow the buffer without bound. One chat-completion event is a few
/// hundred bytes; a whole megabyte is already far outside that.
pub const MAX_EVENT_BYTES: usize = 1024 * 1024;

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
  finished: bool,
}

impl<R: Read> SseStream<R> {
  pub fn new(reader: R) -> Self {
    Self {
      reader: BufReader::with_capacity(8 * 1024, reader),
      finished: false,
    }
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
    let mut data = String::new();
    let mut received = false;
    loop {
      let mut line = String::new();
      if self.reader.read_line(&mut line)? == 0 {
        // EOF. A trailing event without a blank line is still a real event.
        self.finished = true;
        return if received {
          Ok(Some(SseEvent { data }))
        } else {
          Ok(None)
        };
      }
      let trimmed = line.trim_end_matches(['\n', '\r']);
      if trimmed.is_empty() {
        self.finished |= received && data.trim() == DONE;
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
    let line = format!("data: {}\n", "x".repeat(2048));
    let flood = line.repeat(1_000);
    let error = SseStream::new(flood.as_bytes())
      .next_event()
      .expect_err("oversized event must fail, not allocate");
    assert!(error.to_string().contains("exceeds"), "{error}");
  }
}
