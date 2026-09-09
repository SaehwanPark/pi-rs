//! End-to-end tests for `pi-rs trace`.
//!
//! The fixture is a journal written by hand rather than a session produced by a
//! simulated provider run: the point under test is how a recorded trace is read and
//! rendered, and a provider adds latency, a port, and nondeterministic chunking to a
//! question about a pure read path. The one test that cares about the *agreement*
//! between what `run` prints and what `trace` reads back is covered by `tests/run_cli.rs`,
//! which pins the recorded session byte for byte.

use std::{
  fs,
  path::{Path, PathBuf},
  process::{Command, Output},
};

use pi_rs_core::{
  AgentEvent, AssistantDelta, BlobRef, ContextCompactionEpoch, Diagnostic, DiagnosticLevel,
  EpochReason, EventEnvelope, EventMeta, EventSeq, ExternalizedField, ModelCapabilities,
  ModelEndpoint, ModelRef, ReasoningExposure, ReasoningProvenance, RuntimeConfig, SessionEndReason,
  SessionEnded, SessionId, SessionStarted, ToolCallId, ToolCompleted, ToolExecutionState,
  ToolFailed, ToolRequested, ToolStarted, TraceEntry, UserMessage, next_context_epoch,
};
use pi_rs_store::StateLayout;
use tempfile::TempDir;

/// Caps that make no difference to a trace read.
fn caps() -> ModelCapabilities {
  ModelCapabilities {
    text: true,
    images: false,
    tools: true,
    exposed_reasoning: ReasoningExposure::Native,
    context_window: 32_768,
    max_output_tokens: Some(1_024),
  }
}

fn model(name: &str) -> ModelRef {
  let (provider, model) = name.split_once('/').expect("provider/model");
  ModelRef::new(provider, model)
}

/// One fixture session: the journal, plus the session file that puts it in the listing.
struct Fixture {
  id: SessionId,
  root: PathBuf,
}

impl Fixture {
  fn trace(&self) -> PathBuf {
    StateLayout::new(&self.root).trace_path(&self.id)
  }

  fn id_str(&self) -> &str {
    self.id.as_str()
  }
}

/// Write `events` as one session's trace, in order, with assigned sequence numbers.
fn fixture(root: &Path, id: &str, events: &[(u32, AgentEvent)]) -> Fixture {
  let session = SessionId::from_string(id.to_string());
  let entries: Vec<TraceEntry> = events
    .iter()
    .enumerate()
    .map(|(index, (epoch, event))| {
      let mut meta = EventMeta::new(session.clone(), pi_rs_core::TraceId::new());
      meta.seq = Some(pi_rs_core::EventSeq(index as u64 + 1));
      meta.timestamp_ms = 1_700_000_000_000 + index as u64 * 1_000;
      meta.model_epoch = Some(*epoch);
      TraceEntry {
        envelope: EventEnvelope::new(meta, event.clone()),
        redactions: 0,
        raw_payload: false,
        raw_ref: None,
        externalized: Vec::new(),
      }
    })
    .collect();
  fixture_entries(root, id, &entries)
}

/// Write entries that were built by hand, for the cases where the *line* is under
/// test rather than the event: a bounded line records what it left out beside the
/// event it describes.
fn fixture_entries(root: &Path, id: &str, entries: &[TraceEntry]) -> Fixture {
  let id = SessionId::from_string(id.to_string());
  let layout = StateLayout::new(root);
  fs::create_dir_all(layout.sessions_dir()).expect("create sessions dir");
  // The session listing is derived from session files, so a trace alone would be
  // invisible to `list_session_ids`. An empty file is a session with no records.
  fs::write(layout.session_path(&id), "").expect("write session file");
  let mut lines = String::new();
  for entry in entries {
    lines.push_str(&serde_json::to_string(entry).expect("serialize trace entry"));
    lines.push('\n');
  }
  fs::write(layout.trace_path(&id), lines).expect("write trace journal");
  Fixture {
    id,
    root: root.to_path_buf(),
  }
}

/// A session that contains the shapes a reader filters on: prose, reasoning, a
/// successful tool, a failed tool, a warning, and a second model epoch.
fn fixture_events() -> Vec<(u32, AgentEvent)> {
  let agent = model("fake/agent");
  let backup = model("fake/backup");
  let call = ToolCallId::from_string("call-read".to_string());
  let exec = ToolCallId::from_string("call-exec".to_string());
  vec![
    (
      0,
      AgentEvent::SessionStarted(SessionStarted {
        working_dir: "/work".into(),
        model: agent.clone(),
        capabilities: caps(),
        resumed: false,
      }),
    ),
    (
      0,
      AgentEvent::UserMessage(UserMessage {
        text: "fix the build".into(),
        attachments: 0,
      }),
    ),
    (
      0,
      AgentEvent::ReasoningDelta(pi_rs_core::ReasoningDelta {
        text: "the linker ".into(),
        provenance: ReasoningProvenance::Native,
        chunk_index: 0,
      }),
    ),
    (
      0,
      AgentEvent::ReasoningDelta(pi_rs_core::ReasoningDelta {
        text: "is missing -lm".into(),
        provenance: ReasoningProvenance::Native,
        chunk_index: 1,
      }),
    ),
    (
      0,
      AgentEvent::AssistantDelta(AssistantDelta {
        text: "Rebuilt ".into(),
        chunk_index: 0,
      }),
    ),
    (
      0,
      AgentEvent::AssistantDelta(AssistantDelta {
        text: "with -lm; two ".into(),
        chunk_index: 1,
      }),
    ),
    (
      0,
      AgentEvent::AssistantDelta(AssistantDelta {
        text: "targets pass.".into(),
        chunk_index: 2,
      }),
    ),
    (
      0,
      AgentEvent::ToolRequested(ToolRequested {
        call_id: call.clone(),
        name: "read".into(),
        arguments: serde_json::json!({"path": "src/main.rs"}),
        read_only: true,
      }),
    ),
    (
      0,
      AgentEvent::ToolStarted(ToolStarted {
        call_id: call.clone(),
        name: "read".into(),
      }),
    ),
    (
      0,
      AgentEvent::ToolCompleted(ToolCompleted {
        call_id: call.clone(),
        name: "read".into(),
        state: ToolExecutionState::Succeeded,
        duration_ms: 5,
        status: None,
        reduced: false,
        blob: None,
        visible_bytes: 128,
      }),
    ),
    (
      0,
      AgentEvent::ToolRequested(ToolRequested {
        call_id: exec.clone(),
        name: "exec".into(),
        arguments: serde_json::json!({"command": "make"}),
        read_only: false,
      }),
    ),
    (
      0,
      AgentEvent::ToolFailed(ToolFailed {
        call_id: exec,
        name: "exec".into(),
        message: "exit 1".into(),
        duration_ms: 7,
        status: Some(1),
      }),
    ),
    (
      1,
      AgentEvent::ModelEpochStarted(pi_rs_core::ModelEpochStarted {
        epoch: 1,
        model: backup,
        reason: EpochReason::AutomaticFailover,
        capabilities: caps(),
      }),
    ),
    (
      1,
      AgentEvent::ReasoningDelta(pi_rs_core::ReasoningDelta {
        text: "taking over from the primary".into(),
        provenance: ReasoningProvenance::ProviderSummary,
        chunk_index: 0,
      }),
    ),
    (
      0,
      AgentEvent::Diagnostic(Diagnostic {
        level: DiagnosticLevel::Warn,
        message: "context near budget".into(),
      }),
    ),
    (
      0,
      AgentEvent::SessionEnded(SessionEnded {
        reason: SessionEndReason::UserExit,
      }),
    ),
  ]
}

/// Configuration whose store root is `state`, written at `path`.
///
/// No endpoint is consulted: reading a trace must not need a provider.
fn config_at(path: &Path, state: &Path) -> PathBuf {
  let mut config = RuntimeConfig::new(model("fake/agent"), state.to_string_lossy().into_owned());
  config.endpoints.push(ModelEndpoint {
    provider: "fake".into(),
    model: "agent".into(),
    base_url: Some("http://127.0.0.1:1/v1".into()),
    api_key_env: None,
    api_key: None,
    capabilities: caps(),
    max_output_tokens: Some(1_024),
  });
  fs::write(path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
  path.to_path_buf()
}

/// The usual layout: config at the root, traces under `root/state`.
fn config(root: &Path) -> PathBuf {
  config_at(&root.join("config.json"), &root.join("state"))
}

fn trace(config: &Path, extra: &[&str]) -> Output {
  let mut command = Command::new(env!("CARGO_BIN_EXE_pi-rs"));
  command
    .arg("trace")
    .arg("--config")
    .arg(config)
    .args(extra)
    .env_remove("NO_COLOR")
    .env_remove("TERM")
    .env_remove("CLICOLOR")
    .env_remove("CLICOLOR_FORCE");
  command.output().expect("run pi-rs trace")
}

fn stdout(out: &Output) -> String {
  String::from_utf8(out.stdout.clone()).expect("utf-8 stdout")
}

fn stderr(out: &Output) -> String {
  String::from_utf8(out.stderr.clone()).expect("utf-8 stderr")
}

/// Strip ANSI so assertions read as text even under `--color always`.
fn unstyle(text: &str) -> String {
  let mut plain = String::new();
  let mut chars = text.chars().peekable();
  while let Some(char) = chars.next() {
    if char != '\u{1b}' {
      plain.push(char);
      continue;
    }
    // Expect `[`, parameters, then a final byte.
    assert_eq!(chars.next(), Some('['));
    for next in chars.by_ref() {
      if next.is_ascii_alphabetic() || next == '~' {
        break;
      }
    }
  }
  plain
}

/// Display columns, counted by the same rule the renderer uses.
fn columns(line: &str) -> usize {
  pi_rs_tui::display_width(line)
}

/// A temp root holding one fixture session.
fn one_session(id: &str) -> (TempDir, PathBuf, Fixture) {
  let temp = TempDir::new().expect("temp root");
  let session = fixture(&temp.path().join("state"), id, &fixture_events());
  let config = config(temp.path());
  (temp, config, session)
}

#[test]
fn a_trace_reads_the_latest_session_and_says_which() {
  let (_temp, config, session) = one_session("01940000-0000-7000-8000-000000000001");
  let out = trace(&config, &[]);
  assert!(out.status.success(), "{}", stderr(&out));
  let answer = stdout(&out);
  assert!(answer.contains("[answer] Rebuilt with -lm"), "{answer}");
  // The reader must know which session they are being shown.
  assert!(stderr(&out).contains(session.id_str()), "{}", stderr(&out));
  // Metadata belongs on stderr: a piped trace is the transcript and nothing else.
  assert!(
    !answer.contains("entries read"),
    "footer leaked to stdout: {answer}"
  );
}

#[test]
fn a_folded_run_is_one_line_and_keeps_its_provenance_apart() {
  let (_temp, config, _session) = one_session("01940000-0000-7000-8000-000000000002");
  let out = trace(&config, &[]);
  let text = stdout(&out);
  let answers: Vec<String> = text
    .lines()
    .filter(|line| line.starts_with("[answer]"))
    .map(str::to_string)
    .collect();
  assert_eq!(answers.len(), 1, "{text}");
  assert_eq!(answers[0], "[answer] Rebuilt with -lm; two targets pass.");
  // Native reasoning and a provider summary are different claims; folding them
  // together would invent a claim nobody made.
  assert!(
    text.contains("[reasoning] the linker is missing -lm"),
    "{text}"
  );
  assert!(
    text.contains("[provider summary] taking over from the primary"),
    "{text}"
  );
}

#[test]
fn sequence_numbers_address_the_line_a_reader_quotes() {
  let (_temp, config, _session) = one_session("01940000-0000-7000-8000-000000000003");
  let plain = trace(&config, &["--sequence"]);
  let text = unstyle(&stdout(&plain));
  let first = text.lines().next().expect("at least one line");
  assert!(first.starts_with("[1] "), "{first}");
  let answer = text
    .lines()
    .find(|line| line.contains("[answer]"))
    .expect("answer line");
  // The run carries the sequence of its first chunk: that is where the run begins.
  assert!(answer.starts_with("[5] "), "{answer}");
}

/// A trace holding one compaction epoch record, where every record is its own
/// rendered line: no folded fragment run, so "nothing was dropped" is countable.
fn epoch_fixture(root: &Path, id: &str) -> (Vec<(u32, AgentEvent)>, Fixture, BlobRef) {
  let summary = BlobRef::for_bytes(b"summary of the replaced range", Some("text/plain"));
  let mut events = vec![
    (
      0,
      AgentEvent::SessionStarted(SessionStarted {
        working_dir: "/work".into(),
        model: model("fake/agent"),
        capabilities: caps(),
        resumed: false,
      }),
    ),
    (
      0,
      AgentEvent::UserMessage(UserMessage {
        text: "measure the pump".into(),
        attachments: 0,
      }),
    ),
    (
      0,
      AgentEvent::ToolRequested(ToolRequested {
        call_id: ToolCallId::from_string("call-read".to_string()),
        name: "read".into(),
        arguments: serde_json::json!({ "path": "src/pump.rs" }),
        read_only: true,
      }),
    ),
    (
      0,
      AgentEvent::ToolCompleted(ToolCompleted {
        call_id: ToolCallId::from_string("call-read".to_string()),
        name: "read".into(),
        state: ToolExecutionState::Succeeded,
        duration_ms: 5,
        status: None,
        reduced: false,
        blob: None,
        visible_bytes: 64,
      }),
    ),
    (
      0,
      AgentEvent::UserMessage(UserMessage {
        text: "now the valve".into(),
        attachments: 0,
      }),
    ),
    (
      0,
      AgentEvent::SessionEnded(SessionEnded {
        reason: SessionEndReason::UserExit,
      }),
    ),
  ];
  // The record is appended after the range it replaces, and its ordinal comes from
  // the records already in the trace, not from a counter in the writer.
  let replaced_from = 2;
  let replaced_through = 4;
  events.push((
    0,
    AgentEvent::ContextCompactionEpoch(ContextCompactionEpoch {
      context_epoch: next_context_epoch(events.iter().map(|(_, event)| event)),
      replaces_from: EventSeq(replaced_from),
      replaces_through: EventSeq(replaced_through),
      summary: Some(summary.clone()),
    }),
  ));
  let session = fixture(root, id, &events);
  (events, session, summary)
}

#[test]
fn a_compaction_epoch_record_drops_no_canonical_record() {
  let temp = TempDir::new().expect("temp root");
  let (events, _session, summary) = epoch_fixture(
    &temp.path().join("state"),
    "01940000-0000-7000-8000-0000000000e1",
  );
  let config = config(temp.path());
  let out = trace(&config, &["--sequence"]);
  assert!(out.status.success(), "{}", stderr(&out));
  let text = unstyle(&stdout(&out));
  let report = stderr(&out);
  // Counts: every record written was read, and every record read was shown. A
  // dropped range would show up here as fewer shown than read.
  assert!(
    report.contains(&format!(" · {} entries read", events.len())),
    "{report}"
  );
  assert!(
    report.contains(&format!(" · {} shown", events.len())),
    "{report}"
  );
  // The epoch line names the range it replaced, points at the summary instead of
  // containing it, and says out loud that canonical history survived.
  let epoch_line = text
    .lines()
    .find(|line| line.contains("[compact] context epoch 1 ·"))
    .expect("compaction epoch line");
  assert!(epoch_line.contains("replaced 2..4"), "{epoch_line}");
  assert!(epoch_line.contains(&summary.recovery_ref()), "{epoch_line}");
  assert!(
    !epoch_line.contains("summary of the replaced range"),
    "{epoch_line}"
  );
  assert!(
    epoch_line.contains("canonical trace intact"),
    "{epoch_line}"
  );
  // Ordering: the epoch record sorts after the whole range it replaces, because the
  // log assigned it the sequence of the line it was appended to.
  assert!(
    epoch_line.starts_with(&format!("[{}] ", events.len())),
    "{epoch_line}"
  );
  // The replaced range is still addressable and still readable.
  for seq in 2..=4 {
    let prefix = format!("[{seq}] ");
    assert!(
      text.lines().any(|line| line.starts_with(&prefix)),
      "replaced record {seq} is missing from the trace:\n{text}"
    );
  }
  assert!(text.contains("> measure the pump"), "{text}");
  assert!(text.contains("[tool] read"), "{text}");
}

#[test]
fn tools_selects_tool_activity_and_nothing_else() {
  let (_temp, config, _session) = one_session("01940000-0000-7000-8000-000000000004");
  let out = trace(&config, &["--tools"]);
  let text = stdout(&out);
  assert!(text.contains("[tool ok] read"), "{text}");
  assert!(text.contains("[tool failed] exec"), "{text}");
  // A tool call is requested, started, then terminal, and every one of those is tool
  // activity; a continuation line is indented under its entry.
  let kinds: Vec<String> = text
    .lines()
    .map(|line| {
      line
        .strip_prefix(' ')
        .unwrap_or(line)
        .split(' ')
        .next()
        .unwrap_or("")
        .to_string()
    })
    .filter(|label| label.starts_with('['))
    .collect();
  assert!(
    kinds
      .iter()
      .all(|label| label.starts_with("[tool") || label == "[running]"),
    "{kinds:?}"
  );
  assert_eq!(kinds.len(), 5, "{text}");
}

#[test]
fn reasoning_selects_reasoning_and_the_epoch_that_explains_it() {
  let (_temp, config, _session) = one_session("01940000-0000-7000-8000-000000000005");
  let out = trace(&config, &["--reasoning"]);
  let text = stdout(&out);
  assert!(text.contains("[reasoning]"), "{text}");
  assert!(text.contains("[epoch]"), "{text}");
  assert!(!text.contains("[answer]"), "{text}");
  assert!(!text.contains("[tool"), "{text}");
}

#[test]
fn epoch_selects_one_model_and_says_what_it_excluded() {
  let (_temp, config, _session) = one_session("01940000-0000-7000-8000-000000000006");
  let out = trace(&config, &["--epoch", "1"]);
  let text = stdout(&out);
  assert!(text.contains("fake/backup"), "{text}");
  assert!(
    text.contains("[provider summary] taking over from the primary"),
    "{text}"
  );
  assert!(!text.contains("[answer] Rebuilt"), "{text}");

  // An epoch with nothing in it is not the same as an empty session, and a reader
  // who cannot tell them apart draws the wrong conclusion.
  let empty = trace(&config, &["--epoch", "7"]);
  assert!(empty.status.success());
  assert!(stdout(&empty).is_empty());
  assert!(
    stderr(&empty).contains("nothing matched the selection"),
    "{}",
    stderr(&empty)
  );
}

#[test]
fn quiet_leaves_the_trouble_and_drops_the_routine() {
  let (_temp, config, _session) = one_session("01940000-0000-7000-8000-000000000007");
  let out = trace(&config, &["--quiet"]);
  let text = stdout(&out);
  assert!(text.contains("[tool failed] exec"), "{text}");
  assert!(text.contains("context near budget"), "{text}");
  assert!(!text.contains("[tool ok] read"), "{text}");
  assert!(!text.contains("[answer]"), "{text}");
}

#[test]
fn silent_prints_no_transcript_but_still_reports_the_read() {
  let (_temp, config, _session) = one_session("01940000-0000-7000-8000-000000000008");
  let out = trace(&config, &["--silent"]);
  assert!(out.status.success(), "{}", stderr(&out));
  assert_eq!(stdout(&out), "");
  assert!(
    stderr(&out).contains("transcript suppressed"),
    "{}",
    stderr(&out)
  );
}

#[test]
fn a_named_session_is_resolved_by_prefix() {
  let temp = TempDir::new().unwrap();
  let state = temp.path().join("state");
  let older = fixture(
    &state,
    "01940000-0000-7000-8000-0000000000a0",
    &fixture_events(),
  );
  fixture(
    &state,
    "01950000-0000-7000-8000-0000000000b0",
    &fixture_events(),
  );
  let config = config(temp.path());
  let out = trace(&config, &["0194"]);
  assert!(out.status.success(), "{}", stderr(&out));
  assert!(stderr(&out).contains(older.id_str()), "{}", stderr(&out));
  // The newest session would be chosen without the prefix.
  let default = trace(&config, &[]);
  assert!(
    !stderr(&default).contains(older.id_str()),
    "{}",
    stderr(&default)
  );
}

#[test]
fn an_ambiguous_prefix_lists_the_candidates() {
  let temp = TempDir::new().unwrap();
  let state = temp.path().join("state");
  for id in [
    "01940000-0000-7000-8000-0000000000c1",
    "01940000-0000-7000-8000-0000000000c2",
  ] {
    fixture(&state, id, &fixture_events());
  }
  let config = config(temp.path());
  let out = trace(
    &config,
    &["--session", "01940000-0000-7000-8000-0000000000c"],
  );
  assert_eq!(out.status.code(), Some(1));
  let text = stderr(&out);
  assert!(text.contains("2 session ids start with"), "{text}");
  assert!(text.contains("longer prefix"), "{text}");
}

#[test]
fn an_unknown_session_names_the_newest_instead() {
  let (_temp, config, _session) = one_session("01940000-0000-7000-8000-0000000000d0");
  let out = trace(&config, &["0199"]);
  assert_eq!(out.status.code(), Some(1));
  let text = stderr(&out);
  assert!(text.contains("no session id starts with '0199'"), "{text}");
  assert!(text.contains("01940000"), "{text}");
}

#[test]
fn reading_creates_nothing_in_a_missing_store() {
  let temp = TempDir::new().unwrap();
  // The config itself must not create the store: write it beside, not inside, the
  // state root that is absent.
  let config = config_at(
    &temp.path().join("config.json"),
    &temp.path().join("absent"),
  );
  let out = trace(&config, &[]);
  assert_eq!(out.status.code(), Some(1));
  assert!(
    stderr(&out).contains("no sessions recorded"),
    "{}",
    stderr(&out)
  );
  assert!(
    !temp.path().join("absent").exists(),
    "reading a trace created the store root"
  );
}

#[test]
fn a_damaged_line_is_reported_never_silently_skipped() {
  let (_temp, config, session) = one_session("01940000-0000-7000-8000-0000000000e0");
  let mut journal = fs::read_to_string(session.trace()).expect("read journal");
  journal.push_str("{not json}\n");
  fs::write(session.trace(), journal).expect("append damaged line");
  let out = trace(&config, &[]);
  // Tolerating a bad line keeps the command useful; hiding it would not.
  assert!(out.status.success(), "{}", stderr(&out));
  let text = stderr(&out);
  assert!(
    text.contains("1 line(s) in this trace could not be read"),
    "{text}"
  );
  assert!(text.contains("line 17"), "{text}");
  assert!(stdout(&out).contains("[answer]"), "{}", stdout(&out));
}

#[test]
fn width_wraps_without_losing_the_label() {
  let (_temp, config, _session) = one_session("01940000-0000-7000-8000-0000000000e1");
  let out = trace(&config, &["--width", "40"]);
  let text = stdout(&out);
  for line in text.lines() {
    assert!(columns(line) <= 40, "{line}");
    assert!(!line.ends_with(' '), "trailing space: {line:?}");
  }
  let folded = text.lines().find(|line| line.starts_with("[answer]"));
  assert!(folded.is_some(), "{text}");
  // Wrapped continuation lines are not separate entries.
  assert!(text.contains("targets"), "{text}");
}

#[test]
fn colour_is_a_projection_not_a_rewriting() {
  let (_temp, config, _session) = one_session("01940000-0000-7000-8000-0000000000e2");
  let plain = trace(&config, &["--color", "never"]);
  let coloured = trace(&config, &["--color", "always"]);
  let styled = stdout(&coloured);
  assert!(styled.contains("\u{1b}["), "{styled}");
  assert_eq!(unstyle(&styled), stdout(&plain));
}

#[test]
fn the_default_is_plain_text_on_both_streams() {
  let (_temp, config, _session) = one_session("01940000-0000-7000-8000-0000000000e3");
  let out = trace(&config, &[]);
  assert!(!stdout(&out).contains('\u{1b}'));
  assert!(!stderr(&out).contains('\u{1b}'));
}

#[test]
fn a_second_bare_argument_is_a_mistake_not_a_second_session() {
  let (_temp, config, _session) = one_session("01940000-0000-7000-8000-0000000000e4");
  let out = trace(&config, &["0194", "0195"]);
  assert_eq!(out.status.code(), Some(2));
  assert!(
    stderr(&out).contains("expected at most one session id"),
    "{}",
    stderr(&out)
  );
}

#[test]
fn two_category_flags_are_two_different_answers() {
  let (_temp, config, _session) = one_session("01940000-0000-7000-8000-0000000000e5");
  let out = trace(&config, &["--tools", "--reasoning"]);
  assert_eq!(out.status.code(), Some(2));
  assert!(
    stderr(&out).contains("select different categories"),
    "{}",
    stderr(&out)
  );
}

#[test]
fn usage_errors_exit_two_and_trace_help_is_its_own() {
  let binary = env!("CARGO_BIN_EXE_pi-rs");
  let help = Command::new(binary)
    .args(["trace", "--help"])
    .output()
    .expect("run pi-rs trace --help");
  assert!(help.status.success());
  let text = String::from_utf8_lossy(&help.stdout);
  assert!(text.contains("pi-rs trace [session-id]"), "{text}");
  assert!(!text.contains("--prompt"), "{text}");

  let missing = Command::new(binary).arg("trace").output().expect("run");
  assert_eq!(missing.status.code(), Some(2));
  assert!(
    stderr(&missing).contains("--config is required"),
    "{}",
    stderr(&missing)
  );

  let unknown = Command::new(binary)
    .args(["trace", "--prompt", "hi"])
    .output()
    .expect("run");
  assert_eq!(unknown.status.code(), Some(2));
  assert!(
    stderr(&unknown).contains("unknown trace argument '--prompt'"),
    "{}",
    stderr(&unknown)
  );
}

// ---------------------------------------------------------------------------
// A line whose bulk was stored out
// ---------------------------------------------------------------------------

/// Bounding and inspection meet here: the writer moves a field's bytes to the blob
/// store and leaves a preview, and `trace` is the surface that has to keep showing
/// what was actually asked. A reader who sees only `contents` missing would conclude
/// the run asked for a file with no contents.
#[test]
fn a_bounded_line_renders_its_preview_and_names_where_the_bytes_went() {
  let temp = TempDir::new().expect("temp root");
  let reference = format!("blobs/0e/{}", "a".repeat(64));
  let preview = format!(
    "/* generated by glue_generator v4 */\u{2026} [stored 40960 bytes in {reference}, 256 bytes shown]"
  );
  let mut meta = EventMeta::new(
    SessionId::from_string("unused".to_string()),
    pi_rs_core::TraceId::new(),
  );
  meta.seq = Some(pi_rs_core::EventSeq(1));
  let entry = TraceEntry {
    envelope: EventEnvelope::new(
      meta,
      AgentEvent::ToolRequested(ToolRequested {
        call_id: pi_rs_core::ToolCallId::from_string("call-1".to_string()),
        name: "write".into(),
        arguments: serde_json::json!({
          "path": "generated/glue.c",
          "contents": preview,
        }),
        read_only: false,
      }),
    ),
    redactions: 0,
    raw_payload: false,
    raw_ref: None,
    externalized: vec![ExternalizedField {
      field: "event.arguments.contents".to_string(),
      reference: reference.clone(),
      bytes: 40_960,
      inline: 256,
    }],
  };
  let session = fixture_entries(
    &temp.path().join("state"),
    "01940000-0000-7000-8000-00000000000b",
    &[entry],
  );
  let config = config(temp.path());
  // Width zero means "do not wrap", so one entry is exactly one line of output.
  let out = trace(&config, &["--session", session.id_str(), "--width", "0"]);
  assert!(out.status.success(), "{}", stderr(&out));
  let text = stdout(&out);
  let lines: Vec<&str> = text.lines().collect();
  assert_eq!(lines.len(), 1, "a bounded line is still one line: {text}");
  assert!(lines[0].contains("generated/glue.c"), "{text}");
  // The surface truncates a long argument value to keep one request on one line, so
  // the reference itself is cut off here; what must survive is that bytes were stored
  // out and how many. The whole marker, reference included, is in the line.
  assert!(
    lines[0].contains("[stored 40960 bytes"),
    "the reader must see that the contents were stored out, not absent: {text}"
  );
}
