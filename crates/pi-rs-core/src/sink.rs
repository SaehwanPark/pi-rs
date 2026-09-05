//! Event fan-out boundary.
//!
//! The runtime produces events once; several consumers read the same stream.
//! Keeping fan-out in the core contract means the store, the UI, and any
//! optional exporter are peers rather than special cases, and it makes the
//! durability question answerable: a sink states whether it can fail.
//!
//! Ordering expectation: sinks receive events in `seq` order within one
//! session. A sink that fails must not reorder or drop later events for other
//! sinks; the runtime decides whether a durability failure is fatal.

use serde::{Deserialize, Serialize};

use crate::event::EventEnvelope;

/// Sink failure. Free-form because the causes are heterogeneous (I/O, disk
/// full, serialization), but the message must be operator-usable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SinkError(pub String);

impl std::fmt::Display for SinkError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(&self.0)
  }
}

impl std::error::Error for SinkError {}

impl From<std::io::Error> for SinkError {
  fn from(value: std::io::Error) -> Self {
    Self(value.to_string())
  }
}

/// Consumer of the event stream.
pub trait EventSink: Send {
  /// Deliver one event. Implementations must not mutate the event except where
  /// their own contract says so (for example redaction at the durable
  /// boundary).
  fn emit(&mut self, envelope: &EventEnvelope) -> Result<(), SinkError>;

  /// Push buffered events to their final destination.
  fn flush(&mut self) -> Result<(), SinkError>;

  /// `true` when events are only durable after `flush`, so that the runtime
  /// can decide whether to flush on rare but important events.
  fn buffers(&self) -> bool {
    true
  }
}

/// Deliver to several sinks, reporting the first failure while still
/// delivering to the rest.
#[derive(Default)]
pub struct FanOut {
  sinks: Vec<Box<dyn EventSink>>,
}

impl FanOut {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn with(sinks: Vec<Box<dyn EventSink>>) -> Self {
    Self { sinks }
  }

  pub fn push(&mut self, sink: Box<dyn EventSink>) {
    self.sinks.push(sink);
  }

  pub fn len(&self) -> usize {
    self.sinks.len()
  }

  pub fn is_empty(&self) -> bool {
    self.sinks.is_empty()
  }
}

impl EventSink for FanOut {
  fn emit(&mut self, envelope: &EventEnvelope) -> Result<(), SinkError> {
    let mut first_error = None;
    for sink in &mut self.sinks {
      if let Err(error) = sink.emit(envelope) {
        if first_error.is_none() {
          first_error = Some(error);
        }
      }
    }
    first_error.map_or(Ok(()), Err)
  }

  fn flush(&mut self) -> Result<(), SinkError> {
    let mut first_error = None;
    for sink in &mut self.sinks {
      if let Err(error) = sink.flush() {
        if first_error.is_none() {
          first_error = Some(error);
        }
      }
    }
    first_error.map_or(Ok(()), Err)
  }

  fn buffers(&self) -> bool {
    self.sinks.iter().any(|sink| sink.buffers())
  }
}

/// Discards events. Used when durable capture is intentionally off.
#[derive(Default)]
pub struct NullSink;

impl EventSink for NullSink {
  fn emit(&mut self, _envelope: &EventEnvelope) -> Result<(), SinkError> {
    Ok(())
  }

  fn flush(&mut self) -> Result<(), SinkError> {
    Ok(())
  }

  fn buffers(&self) -> bool {
    false
  }
}

/// Keeps events in memory. Used by tests, the UI replay buffer, and
/// short-lived non-durable modes.
#[derive(Default)]
pub struct MemorySink {
  events: Vec<EventEnvelope>,
}

impl MemorySink {
  pub fn events(&self) -> &[EventEnvelope] {
    &self.events
  }

  pub fn take(&mut self) -> Vec<EventEnvelope> {
    std::mem::take(&mut self.events)
  }
}

impl EventSink for MemorySink {
  fn emit(&mut self, envelope: &EventEnvelope) -> Result<(), SinkError> {
    self.events.push(envelope.clone());
    Ok(())
  }

  fn flush(&mut self) -> Result<(), SinkError> {
    Ok(())
  }

  fn buffers(&self) -> bool {
    false
  }
}

#[cfg(test)]
mod tests {
  use crate::{
    event::{AgentEvent, Diagnostic, DiagnosticLevel, EventMeta},
    ids::{SessionId, TraceId},
  };

  use super::*;

  fn envelope(message: &str) -> EventEnvelope {
    EventEnvelope::new(
      EventMeta::new(SessionId::new(), TraceId::new()),
      AgentEvent::Diagnostic(Diagnostic {
        level: DiagnosticLevel::Info,
        message: message.to_string(),
      }),
    )
  }

  /// Sink used to prove that one failing sink neither stops delivery nor
  /// hides its error.
  struct AlwaysFails;

  impl EventSink for AlwaysFails {
    fn emit(&mut self, _envelope: &EventEnvelope) -> Result<(), SinkError> {
      Err(SinkError("disk full".into()))
    }

    fn flush(&mut self) -> Result<(), SinkError> {
      Err(SinkError("disk full".into()))
    }
  }

  /// Counts deliveries through a shared cell so that a failing peer sink can
  /// be shown not to stop them.
  #[derive(Clone, Default)]
  struct CountingSink {
    counter: std::sync::Arc<std::sync::Mutex<usize>>,
  }

  impl EventSink for CountingSink {
    fn emit(&mut self, _envelope: &EventEnvelope) -> Result<(), SinkError> {
      *self.counter.lock().unwrap() += 1;
      Ok(())
    }

    fn flush(&mut self) -> Result<(), SinkError> {
      Ok(())
    }

    fn buffers(&self) -> bool {
      true
    }
  }

  #[test]
  fn fan_out_delivers_to_every_sink_and_reports_failure() {
    let counter = std::sync::Arc::new(std::sync::Mutex::new(0usize));
    let mut fan = FanOut::with(vec![
      Box::new(AlwaysFails),
      Box::new(CountingSink {
        counter: counter.clone(),
      }),
    ]);
    let error = fan.emit(&envelope("hello")).unwrap_err();
    assert_eq!(error.to_string(), "disk full");
    assert_eq!(
      *counter.lock().unwrap(),
      1,
      "a failing peer sink must not stop delivery"
    );
    assert_eq!(fan.flush().unwrap_err().to_string(), "disk full");
    assert!(
      fan.buffers(),
      "one buffering sink makes the fan-out buffering"
    );
    assert_eq!(fan.len(), 2);

    fan.push(Box::new(MemorySink::default()));
    let empty = FanOut::new();
    assert!(empty.is_empty());
  }

  #[test]
  fn memory_sink_preserves_order_and_null_sink_discards() {
    let mut memory = MemorySink::default();
    memory.emit(&envelope("first")).unwrap();
    memory.emit(&envelope("second")).unwrap();
    memory.flush().unwrap();
    let events = memory.take();
    assert_eq!(events.len(), 2);
    let AgentEvent::Diagnostic(first) = &events[0].event else {
      panic!("expected diagnostic");
    };
    assert_eq!(first.message, "first");

    let mut null = NullSink;
    null.emit(&envelope("gone")).unwrap();
    null.flush().unwrap();
    assert!(!null.buffers());
  }
}
