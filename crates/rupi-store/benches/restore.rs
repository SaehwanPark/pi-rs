//! Large-session restore benchmark.
//!
//! Why this exists: session resume and context reconstruction occur at process startup
//! whenever a user runs `rupi run --resume <id>` or launches the interactive surface
//! on an existing session. Phase 4 of the roadmap requires verifying that restore time
//! remains low for large historical sessions and that checkpoint barriers bound context
//! hydration cost in practice.
//!
//! The benchmark measures `rupi_store::session_log::restore`, which parses the durable
//! JSONL log, tracks sequence continuity, records checkpoint barriers, and restores
//! active message state.
//!
//! Run: `cargo bench -p rupi-store --bench restore`
//! Or:  `bench/large_session.sh [--iterations <N>] [--json <path>]`

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use rupi_core::{
  EventSeq, ModelRef, SessionId,
  context::{CAPSULE_SCHEMA_VERSION, CapsuleDecision, ContextCapsule},
  ids::{CheckpointId, EventId, TurnId, uuidv7},
  message::Message,
  session::{
    SESSION_SCHEMA_VERSION, SessionCheckpointRecord, SessionHeader, SessionMessage, SessionRecord,
  },
};
use rupi_store::{SessionLog, restore};
use tempfile::TempDir;

/// Latency budgets in microseconds (us) per session restore.
///
/// Budgets are set with ample headroom (~5x baseline on standard developer hardware)
/// to tolerate noise while firmly catching algorithmic or IO regressions.
const BUDGETS: &[(&str, f64)] = &[
  ("restore_small_10_turns", 1_000.0),
  ("restore_medium_100_turns", 4_000.0),
  ("restore_large_500_turns", 15_000.0),
  ("restore_large_500_turns_checkpoints", 15_000.0),
  ("restore_large_1000_turns_checkpoints", 30_000.0),
];

const DEFAULT_ITERATIONS: usize = 50;

fn main() -> ExitCode {
  let args: Vec<String> = env::args().skip(1).collect();
  if args.iter().any(|a| a == "--help" || a == "-h") {
    println!("Usage: restore [--iterations <N>] [--json <path>]");
    return ExitCode::SUCCESS;
  }
  let iterations = arg_value(&args, "--iterations")
    .and_then(|v| v.parse::<usize>().ok())
    .filter(|n| *n > 0)
    .unwrap_or(DEFAULT_ITERATIONS);
  let json_out = arg_value(&args, "--json").map(String::from);

  let temp = TempDir::new().expect("create temp dir for benchmark sessions");
  let test_cases = generate_cases(temp.path());

  println!("Session restore benchmark (rupi-store):");
  let mut measured: Vec<(&str, f64, usize, usize)> = Vec::new();

  for (name, path, expected_total, expected_messages) in &test_cases {
    let mut samples = Vec::with_capacity(iterations);
    let mut restored_total = 0;
    let mut restored_messages = 0;

    for _ in 0..iterations {
      let start = Instant::now();
      let res = restore(path).expect("restore session log");
      let elapsed_us = start.elapsed().as_secs_f64() * 1_000_000.0;
      samples.push(elapsed_us);
      restored_total = res.total_records;
      restored_messages = res.messages.len();
    }

    assert_eq!(
      restored_total, *expected_total,
      "record count mismatch for {name}"
    );
    assert_eq!(
      restored_messages, *expected_messages,
      "active messages mismatch for {name}"
    );

    measured.push((
      name,
      median(&mut samples),
      restored_total,
      restored_messages,
    ));
  }

  let mut breached = Vec::new();
  for (name, us, total, messages) in &measured {
    let budget = BUDGETS
      .iter()
      .find(|(case, _)| case == name)
      .map(|(_, budget)| *budget)
      .unwrap_or(0.0);
    let status = if *us > budget {
      breached.push((*name, *us, budget));
      "FAIL"
    } else {
      "ok"
    };
    println!(
      "  {name:<38} {us:>8.2} us  budget {budget:>7.0} us  {status}  (records: {total:>4}, active msgs: {messages:>3})"
    );
  }

  if let Some(path) = json_out {
    let mut json = String::from("{\n  \"iterations\": ");
    json.push_str(&iterations.to_string());
    json.push_str(",\n  \"cases\": {\n");
    for (index, (name, us, total, messages)) in measured.iter().enumerate() {
      let budget = BUDGETS
        .iter()
        .find(|(case, _)| case == name)
        .map(|(_, budget)| *budget)
        .unwrap_or(0.0);
      json.push_str(&format!(
        "    \"{name}\": {{\"median_us\": {:.3}, \"budget_us\": {:.3}, \"total_records\": {}, \"active_messages\": {}}}",
        us, budget, total, messages
      ));
      if index + 1 < measured.len() {
        json.push(',');
      }
      json.push('\n');
    }
    json.push_str("  }\n}\n");
    if let Err(error) = fs::write(path, json) {
      eprintln!("could not write results: {error}");
      return ExitCode::FAILURE;
    }
  }

  if breached.is_empty() {
    ExitCode::SUCCESS
  } else {
    for (name, us, budget) in breached {
      eprintln!("budget exceeded: {name} measured {us:.2} us, budget {budget:.0} us");
    }
    ExitCode::FAILURE
  }
}

fn arg_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
  args
    .windows(2)
    .find(|w| w[0] == flag)
    .map(|w| w[1].as_str())
}

fn median(values: &mut [f64]) -> f64 {
  values.sort_by(|a, b| a.partial_cmp(b).unwrap());
  let mid = values.len() / 2;
  if values.len() % 2 == 0 {
    (values[mid - 1] + values[mid]) / 2.0
  } else {
    values[mid]
  }
}

fn generate_cases(root: &Path) -> Vec<(&'static str, PathBuf, usize, usize)> {
  vec![
    // 10 turns: 1 header + 10 user + 10 assistant = 21 records, 20 active messages
    (
      "restore_small_10_turns",
      create_session_file(root, "small_10", 10, None),
      21,
      20,
    ),
    // 100 turns: 1 header + 200 messages = 201 records, 200 active messages
    (
      "restore_medium_100_turns",
      create_session_file(root, "medium_100", 100, None),
      201,
      200,
    ),
    // 500 turns: 1 header + 1000 messages = 1001 records, 1000 active messages
    (
      "restore_large_500_turns",
      create_session_file(root, "large_500", 500, None),
      1001,
      1000,
    ),
    // 500 turns with checkpoint every 50 turns (9 checkpoints):
    // 1 header + 1000 messages + 9 checkpoints = 1010 records.
    // Active messages after the last checkpoint at turn 451: 50 turns * 2 = 100 messages!
    (
      "restore_large_500_turns_checkpoints",
      create_session_file(root, "large_500_cp", 500, Some(50)),
      1010,
      100,
    ),
    // 1000 turns with checkpoint every 100 turns (9 checkpoints):
    // 1 header + 2000 messages + 9 checkpoints = 2010 records.
    // Active messages after the last checkpoint at turn 901: 100 turns * 2 = 200 messages!
    (
      "restore_large_1000_turns_checkpoints",
      create_session_file(root, "large_1000_cp", 1000, Some(100)),
      2010,
      200,
    ),
  ]
}

fn create_session_file(
  root: &Path,
  name: &str,
  turns: usize,
  checkpoint_interval: Option<usize>,
) -> PathBuf {
  let file_path = root.join(format!("{name}.jsonl"));
  let session_id = SessionId::from_string(uuidv7());
  let header = SessionHeader {
    session_id,
    version: SESSION_SCHEMA_VERSION,
    started_at_ms: 1_700_000_000_000,
    working_dir: "/repo".into(),
    model: ModelRef::new("benchmark", "model"),
    parent_session: None,
    branched_from_event: None,
    imported_from: None,
  };

  let mut log = SessionLog::create(&file_path, header).expect("create benchmark session log");
  let mut seq = 1u64;

  for turn_idx in 1..=turns {
    // If checkpoint interval hit and not turn 1, record checkpoint barrier
    if let Some(interval) = checkpoint_interval {
      if turn_idx > 1 && (turn_idx - 1) % interval == 0 {
        log
          .append(&SessionRecord::CheckpointBarrier(SessionCheckpointRecord {
            checkpoint_id: CheckpointId::new(),
            capsule_version: CAPSULE_SCHEMA_VERSION,
            context_epoch: 0,
            capsule_path: format!("checkpoints/cp-{turn_idx}.json"),
            capsule: ContextCapsule {
              version: CAPSULE_SCHEMA_VERSION,
              objective: format!("Objective at turn {turn_idx}"),
              completed_work: vec![format!("Work up to turn {turn_idx}")],
              decisions: vec![CapsuleDecision {
                decision: "Continue with test".into(),
                rationale: "benchmark".into(),
              }],
              constraints: vec![],
              current_state: "active".into(),
              artifacts: vec![],
              unresolved: vec![],
              next_actions: vec!["Process next turn".into()],
            },
          }))
          .expect("append checkpoint barrier");
      }
    }

    // User message
    let user_msg = SessionRecord::Message(SessionMessage {
      turn_id: TurnId::new(),
      role: rupi_core::message::Role::User,
      message: Message::user(format!("User query {turn_idx}")),
      epoch: 0,
      model: ModelRef::new("benchmark", "model"),
      event_id: EventId::new(),
      seq: Some(EventSeq(seq)),
      external_context: None,
    });
    log.append(&user_msg).expect("append user message");
    seq += 1;

    // Assistant message
    let assistant_msg = SessionRecord::Message(SessionMessage {
      turn_id: TurnId::new(),
      role: rupi_core::message::Role::Assistant,
      message: Message::assistant(format!("Assistant response {turn_idx}")),
      epoch: 0,
      model: ModelRef::new("benchmark", "model"),
      event_id: EventId::new(),
      seq: Some(EventSeq(seq)),
      external_context: None,
    });
    log
      .append(&assistant_msg)
      .expect("append assistant message");
    seq += 1;
  }

  file_path
}
