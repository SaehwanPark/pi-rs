//! Provenance is a claim, so it has to survive every hop that claims to keep it.
//!
//! The claim under test is narrow and absolute: reasoning-like text never travels
//! without the statement of where it came from, and no serialization step may weaken
//! that statement — not into a bare string, not into a default, and not into `native`,
//! which is the one weakening that turns inference into reported thought.
//!
//! Each hop gets its own test because the hops fail differently. A journal that
//! defaulted a missing provenance would pass a test written only against the writer,
//! and a renderer that labelled every block "reasoning" would pass a test written only
//! against the event.

use pi_rs_core::{
  capability::ModelRef,
  event::{AgentEvent, EventEnvelope, EventMeta, ReasoningDelta, UserMessage},
  ids::{SessionId, SpanId, TraceId, TurnId, uuidv7},
  message::{ContentBlock, Message, Role},
  provenance::{ReasoningChunk, ReasoningProvenance},
  session::{SESSION_SCHEMA_VERSION, SessionHeader},
};
use pi_rs_store::{Store, TempDir, TraceJournal, WritePolicy};
use pi_rs_tui::{
  // The render role, not the message role: one says how a line is styled, the other who
  // spoke. They share a name in the two crates, and the distinction is the point here.
  style::Role as RenderRole,
  transcript::{TranscriptOptions, reasoning_role, render_event},
};

/// All four claims, in the order the type declares them.
const ALL: [ReasoningProvenance; 4] = [
  ReasoningProvenance::Native,
  ReasoningProvenance::ProviderSummary,
  ReasoningProvenance::Declared,
  ReasoningProvenance::Reconstructed,
];

fn model() -> ModelRef {
  ModelRef::new("test", "primary")
}

fn header(session: &SessionId) -> SessionHeader {
  SessionHeader {
    session_id: session.clone(),
    version: SESSION_SCHEMA_VERSION,
    started_at_ms: 1,
    working_dir: "/work".into(),
    model: model(),
    parent_session: None,
    branched_from_event: None,
    imported_from: None,
  }
}

fn meta(session: &SessionId, turn: &TurnId) -> EventMeta {
  EventMeta {
    event_id: pi_rs_core::ids::EventId::new(),
    session_id: session.clone(),
    turn_id: Some(turn.clone()),
    seq: None,
    timestamp_ms: 1,
    model_epoch: Some(0),
    model: Some(model()),
    tool_call_id: None,
    parent_event_id: None,
    trace_id: TraceId::new(),
    span_id: SpanId::new(),
  }
}

fn reasoning(provenance: ReasoningProvenance) -> AgentEvent {
  AgentEvent::ReasoningDelta(ReasoningDelta {
    text: "because".into(),
    provenance,
    chunk_index: 0,
  })
}

fn rendered(event: &AgentEvent) -> String {
  render_event(event, &TranscriptOptions::default())
    .iter()
    .map(|line| line.plain())
    .collect::<Vec<_>>()
    .join("\n")
}

#[test]
fn every_provenance_survives_the_journal() {
  let root = TempDir::new("provenance-journal");
  let store = Store::new(root.path(), WritePolicy::default());
  let session_id = SessionId::from_string(uuidv7());
  let turn = TurnId::new();
  let mut session = store.begin(header(&session_id)).expect("begin session");

  for provenance in ALL {
    let mut envelope = EventEnvelope::new(meta(&session_id, &turn), reasoning(provenance));
    let encoded = serde_json::to_string(&envelope.event).expect("encode");
    assert!(
      encoded.contains(provenance.as_str()),
      "{provenance:?} must be written under its own label: {encoded}"
    );
    session.emit(&mut envelope).expect("emit reasoning delta");
  }
  session.flush().expect("flush");

  let recorded = TraceJournal::read(session.trace_path())
    .expect("read trace")
    .items;
  assert_eq!(recorded.len(), ALL.len(), "one delta per claim");
  for (entry, provenance) in recorded.iter().zip(ALL) {
    match &entry.envelope.event {
      AgentEvent::ReasoningDelta(delta) => {
        assert_eq!(delta.provenance, provenance, "the claim changed on disk");
        assert_eq!(delta.text, "because");
      }
      other => panic!("expected a reasoning delta, found {other:?}"),
    }
  }
}

#[test]
fn every_provenance_is_named_when_rendered() {
  let mut lines = Vec::new();
  for provenance in ALL {
    let line = rendered(&reasoning(provenance));
    assert!(
      line.contains(provenance.label()),
      "{provenance:?} must be labelled by name: {line}"
    );
    lines.push(line);
  }
  // Four claims, four distinct renderings. Two that collapse into one line would
  // pass any test that only looked at one of them.
  let mut unique = lines.clone();
  unique.sort();
  unique.dedup();
  assert_eq!(
    unique.len(),
    ALL.len(),
    "labels are not distinguishable: {lines:?}"
  );

  // Roles, not only words: colour is how a reader separates these at a glance, and one
  // role reused for two claims is how a provider summary starts looking like reasoning
  // the model emitted.
  for (index, left) in ALL.iter().enumerate() {
    for right in &ALL[index + 1..] {
      assert_ne!(
        reasoning_role(*left),
        reasoning_role(*right),
        "{left:?} and {right:?} would be styled identically"
      );
    }
  }
  assert_eq!(
    reasoning_role(ReasoningProvenance::Native),
    RenderRole::ReasoningNative,
    "native reasoning has its own role, and nothing else borrows it"
  );
}

#[test]
fn only_native_reasoning_may_be_called_emitted_thought() {
  // The prose a reader sees is generated from this predicate, so it is where the
  // difference between "the model showed its work" and "something summarised the work"
  // is actually enforced.
  for provenance in ALL {
    assert_eq!(
      provenance.is_not_hidden_reasoning(),
      provenance != ReasoningProvenance::Native,
      "{provenance:?} must be described as emitted reasoning only if it is native",
    );
  }
  // Inference is the narrowest claim of all: only reconstructed text is pi-rs speaking.
  assert_eq!(
    ALL
      .iter()
      .filter(|provenance| provenance.is_inferred())
      .copied()
      .collect::<Vec<_>>()
      .as_slice(),
    &[ReasoningProvenance::Reconstructed]
  );
}

#[test]
fn a_session_message_keeps_the_reasoning_chunk_and_its_source() {
  let root = TempDir::new("provenance-session");
  let store = Store::new(root.path(), WritePolicy::default());
  let session_id = SessionId::from_string(uuidv7());
  let turn = TurnId::new();
  let mut session = store.begin(header(&session_id)).expect("begin session");

  let chunk = ReasoningChunk::new("weighing two options", ReasoningProvenance::ProviderSummary)
    .with_source("openai/response.summary");
  let message = Message::new(
    Role::Assistant,
    vec![
      ContentBlock::Reasoning(chunk.clone()),
      ContentBlock::text("the answer"),
    ],
  );
  let mut introduced = EventEnvelope::new(
    meta(&session_id, &turn),
    AgentEvent::UserMessage(UserMessage {
      text: "why".into(),
      attachments: 0,
    }),
  );
  session.emit(&mut introduced).expect("emit turn start");
  let stored = session
    .append_message(&turn, &message, 0, &model(), &introduced)
    .expect("append message");

  let restored = store
    .restore(session.id())
    .expect("restore session")
    .messages;
  assert_eq!(restored.len(), 1);
  assert_eq!(restored[0].event_id, stored.event_id);
  let found = restored[0]
    .message
    .content
    .iter()
    .find_map(|block| match block {
      ContentBlock::Reasoning(chunk) => Some(chunk.clone()),
      _ => None,
    })
    .expect("the reasoning chunk must still be there");
  assert_eq!(
    found, chunk,
    "provenance or source changed on the round trip"
  );
  assert_eq!(
    found.provenance,
    ReasoningProvenance::ProviderSummary,
    "a summary must not come back as native reasoning"
  );
}

#[test]
fn a_line_that_lost_its_claim_is_refused_rather_than_read_as_native() {
  // The dangerous reading is the convenient one: a trace line whose claim went missing
  // must not be read as native reasoning. So the claim is a required part of the line,
  // and the question is what the reader does when the file no longer has one.
  //
  // The line comes from the writer rather than from the test. A trace line carries its
  // event fields beside the envelope, so a test written against a shape the writer never
  // produces would prove nothing about the file that is actually on disk.
  let root = TempDir::new("provenance-missing");
  let store = Store::new(root.path(), WritePolicy::default());
  let session_id = SessionId::from_string(uuidv7());
  let turn = TurnId::new();
  let mut session = store.begin(header(&session_id)).expect("begin session");
  let mut envelope = EventEnvelope::new(
    meta(&session_id, &turn),
    reasoning(ReasoningProvenance::Native),
  );
  session.emit(&mut envelope).expect("emit reasoning delta");
  session.flush().expect("flush");

  let path = session.trace_path();
  let written = std::fs::read_to_string(path).expect("read the trace back");
  let mut stripped: serde_json::Value =
    serde_json::from_str(written.trim()).expect("the trace line is JSON");
  let removed = stripped
    .as_object_mut()
    .expect("the trace line is an object")
    .remove("provenance");
  assert_eq!(
    removed,
    Some(serde_json::json!("native")),
    "the writer must state the claim on the line itself: {written}"
  );
  let claimless = stripped.to_string();

  // Read it back through the journal, not just the type. What matters is that a session
  // opened later cannot mistake this line for reasoning the model emitted.
  std::fs::write(path, format!("{claimless}\n")).expect("write the claimless line");
  let report = TraceJournal::read(path).expect("the journal must still open");
  assert_eq!(
    report.malformed, 1,
    "a line with no claim is a malformed line, and that has to be counted: {claimless}"
  );
  assert!(
    report
      .items
      .iter()
      .all(|entry| !matches!(entry.envelope.event, AgentEvent::ReasoningDelta(_))),
    "a claimless line must not arrive as a reasoning delta carrying some default claim"
  );
}
