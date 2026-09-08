//! Failover through the real binary: a primary that cannot serve, a backup that
//! answers, and a durable record of which model produced what.
//!
//! The fake provider lives in `tests/fake_provider`, shared with `tests/run_cli.rs`:
//! the two copies that came before it had drifted into the same nondeterminism, so the
//! harness is now one file with the rules written down where they can be argued about.

mod fake_provider;

use std::{
  fs,
  path::{Path, PathBuf},
  process::{Command, Output},
};

use pi_rs_core::{ModelCapabilities, ModelEndpoint, ModelRef, ReasoningExposure, RuntimeConfig};
use pi_rs_store::{StateLayout, TraceJournal};
use tempfile::TempDir;

use fake_provider::{FakeServer, read_written, text_response, tool_call, unavailable};

/// Primary `fake/agent`, backup `fake/standby` (or a backup that cannot be built).
fn write_config(root: &Path, primary_url: &str, backup: Backup) -> PathBuf {
  write_config_from(root, &config_at(root, primary_url, backup))
}

fn write_config_from(root: &Path, config: &RuntimeConfig) -> PathBuf {
  let path = root.join("config.json");
  fs::write(&path, serde_json::to_vec_pretty(config).unwrap()).unwrap();
  path
}

fn config_at(root: &Path, primary_url: &str, backup: Backup) -> RuntimeConfig {
  let state = root.join("state");
  let mut config = RuntimeConfig::new(ModelRef::new("fake", "agent"), state.to_string_lossy());
  config
    .endpoints
    .push(endpoint("agent", Some(primary_url.into())));
  match backup {
    Backup::Served(url) => {
      config.backup = Some(ModelRef::new("fake", "standby"));
      config.endpoints.push(endpoint("standby", Some(url)));
    }
    Backup::Unbuildable => {
      // Addresses no HTTP endpoint: `RuntimeConfig::validate` accepts it, and only
      // constructing the adapter finds out.
      config.backup = Some(ModelRef::new("fake", "broken"));
      config.endpoints.push(endpoint("broken", None));
    }
    Backup::None => {}
  }
  config
}

enum Backup {
  Served(String),
  Unbuildable,
  None,
}

fn endpoint(model: &str, base_url: Option<String>) -> ModelEndpoint {
  ModelEndpoint {
    provider: "fake".into(),
    model: model.into(),
    base_url,
    api_key_env: None,
    api_key: None,
    capabilities: ModelCapabilities {
      text: true,
      images: false,
      tools: true,
      exposed_reasoning: ReasoningExposure::Native,
      context_window: 32_768,
      max_output_tokens: Some(1_024),
    },
    max_output_tokens: Some(1_024),
  }
}

fn run(config: &Path, cwd: &Path, prompt: &str) -> Output {
  Command::new(env!("CARGO_BIN_EXE_pi-rs"))
    .args(["run", "--config"])
    .arg(config)
    .arg("--cwd")
    .arg(cwd)
    .args(["--prompt", prompt])
    .output()
    .expect("run pi-rs")
}

/// One recorded event with the attribution the stage gate asks about.
///
/// `epoch`/`model` are envelope attribution: which model was in charge when the
/// event was written. `about_epoch`/`about_model` are what the event *describes*.
/// They differ for a transition event, which the leaving epoch records.
struct Recorded {
  kind: String,
  call_id: Option<String>,
  epoch: Option<u32>,
  model: Option<String>,
  about_epoch: Option<u64>,
  about_model: Option<String>,
}

fn trace(state: &Path) -> Vec<Recorded> {
  let layout = StateLayout::new(state);
  let session_id = layout
    .list_session_ids()
    .unwrap()
    .pop()
    .expect("session id");
  TraceJournal::read(&layout.trace_path(&session_id))
    .unwrap()
    .items
    .into_iter()
    .map(|entry| {
      let payload = serde_json::to_value(&entry.envelope.event).unwrap();
      Recorded {
        kind: payload
          .get("type")
          .and_then(|value| value.as_str())
          .unwrap_or("?")
          .to_string(),
        call_id: payload
          .get("call_id")
          .and_then(|value| value.as_str())
          .map(str::to_string),
        epoch: entry.envelope.meta.model_epoch,
        model: entry.envelope.meta.model.as_ref().map(ModelRef::as_key),
        about_epoch: payload.get("epoch").and_then(|value| value.as_u64()),
        about_model: payload
          .get("model")
          .and_then(|value| value.as_str())
          .map(str::to_string),
      }
    })
    .collect()
}

/// A session whose primary is down: two refused requests, then whatever the backup
/// endpoint can do.
struct Takeover {
  _temp: TempDir,
  config: PathBuf,
  workspace: PathBuf,
  state: PathBuf,
  primary: FakeServer,
  standby: FakeServer,
}

/// Primary answers 503 twice; the standby serves `answers`.
fn takeover(answers: Vec<String>) -> Takeover {
  takeover_with(answers, |_| {})
}

/// As [`takeover`], with the standby's declared capabilities narrowed.
///
/// The capability gate compares claims, so a test about that gate is a test about two
/// endpoint entries. The primary's own claims stay as the session's requirement.
fn takeover_with_narrow_backup(
  answers: Vec<String>,
  narrow: impl FnOnce(&mut ModelCapabilities),
) -> Takeover {
  takeover_with(answers, |config| {
    let standby = config
      .endpoints
      .iter_mut()
      .find(|entry| entry.model == "standby")
      .expect("the standby endpoint");
    narrow(&mut standby.capabilities);
  })
}

/// Primary answers 503 twice; the standby serves `answers`; `tune` edits the config
/// before it is written.
fn takeover_with(answers: Vec<String>, tune: impl FnOnce(&mut RuntimeConfig)) -> Takeover {
  let primary = FakeServer::answer(vec![unavailable(), unavailable()]);
  let standby = FakeServer::answer(answers);
  let url = standby.base_url();
  takeover_at(
    TempDir::new().expect("state root"),
    primary,
    standby,
    Backup::Served(url),
    tune,
  )
}

/// Primary answers 503 twice; the backup endpoint cannot build an adapter.
fn takeover_into_unbuildable() -> Takeover {
  let primary = FakeServer::answer(vec![unavailable(), unavailable()]);
  let standby = FakeServer::answer(Vec::new());
  takeover_at(
    TempDir::new().expect("state root"),
    primary,
    standby,
    Backup::Unbuildable,
    |_| {},
  )
}

fn takeover_at(
  temp: TempDir,
  primary: FakeServer,
  standby: FakeServer,
  backup: Backup,
  tune: impl FnOnce(&mut RuntimeConfig),
) -> Takeover {
  let root = temp.path().to_path_buf();
  let workspace = root.join("workspace");
  fs::create_dir_all(&workspace).expect("workspace");
  let mut config = config_at(&root, &primary.base_url(), backup);
  tune(&mut config);
  let config = write_config_from(&root, &config);
  Takeover {
    _temp: temp,
    config,
    workspace,
    state: root.join("state"),
    primary,
    standby,
  }
}

#[test]
fn a_down_primary_hands_the_turn_to_the_backup() {
  let scene = takeover(vec![text_response("served by the standby")]);
  let output = run(&scene.config, &scene.workspace, "go");
  let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
  let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
  assert!(output.status.success(), "{stderr}");
  assert_eq!(stdout, "served by the standby\n");
  // Takeover is a rare event, so the default surface reports it without `--verbose`.
  assert!(stderr.contains("[failover]"), "{stderr}");
  assert!(stderr.contains("fake/agent"), "{stderr}");
  assert!(stderr.contains("fake/standby"), "{stderr}");
  assert!(stderr.contains("provider_unavailable"), "{stderr}");
  // The primary got its full attempt budget, and only then did the work move.
  assert_eq!(scene.primary.requests().len(), 2, "retry, then yield");
  assert_eq!(scene.standby.requests().len(), 1);
}

#[test]
fn a_failover_records_the_epoch_that_served_the_answer() {
  let scene = takeover(vec![text_response("served by the standby")]);
  let output = run(&scene.config, &scene.workspace, "go");
  let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
  assert!(output.status.success(), "{stderr}");

  let events = trace(&scene.state);
  let requests: Vec<&Recorded> = events
    .iter()
    .filter(|event| event.kind == "model_request_started")
    .collect();
  assert_eq!(
    requests.len(),
    3,
    "two primary attempts, one takeover: {stderr}"
  );
  for attempt in &requests[..2] {
    assert_eq!(attempt.epoch, Some(0), "primary attempts stay in epoch 0");
    assert_eq!(attempt.model.as_deref(), Some("fake/agent"));
  }
  let served = requests[2];
  assert_eq!(served.epoch, Some(1), "the backup opens epoch 1");
  assert_eq!(served.model.as_deref(), Some("fake/standby"));

  let turn = events
    .iter()
    .find(|event| event.kind == "turn_completed")
    .expect("turn completed");
  assert_eq!(
    turn.epoch,
    Some(1),
    "the turn belongs to the model that finished it"
  );
  assert_eq!(turn.model.as_deref(), Some("fake/standby"));

  let epochs: Vec<&Recorded> = events
    .iter()
    .filter(|event| event.kind == "model_epoch_started")
    .collect();
  assert_eq!(epochs.len(), 2, "initial epoch plus the takeover");
  assert_eq!(epochs[0].about_epoch, Some(0));
  assert_eq!(epochs[0].about_model.as_deref(), Some("fake/agent"));
  assert_eq!(epochs[1].about_epoch, Some(1));
  assert_eq!(epochs[1].about_model.as_deref(), Some("fake/standby"));
  // The transition is recorded by the epoch it leaves, so its own attribution is
  // epoch 0 while it describes epoch 1. Reading the two as one field would either
  // lose who caused the switch or pretend the new model announced itself.
  assert_eq!(epochs[1].epoch, Some(0));
}

#[test]
fn an_untouched_backup_is_never_built() {
  // This backup endpoint cannot be built at all. If a configured backup were
  // initialized at startup, the session would fail before it began; deferral means a
  // healthy primary is entirely unaffected by a backup it never uses.
  let temp = TempDir::new().expect("state root");
  let root = temp.path();
  let primary = FakeServer::answer(vec![text_response("primary was enough")]);
  let workspace = root.join("workspace");
  fs::create_dir_all(&workspace).expect("workspace");
  let config = write_config(root, &primary.base_url(), Backup::Unbuildable);
  let output = run(&config, &workspace, "go");
  let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
  let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
  assert!(output.status.success(), "{stderr}");
  assert_eq!(stdout, "primary was enough\n");
  assert!(!stderr.contains("initialized"), "{stderr}");
  assert!(!stderr.contains("fake/broken"), "{stderr}");
  assert_eq!(primary.requests().len(), 1);
}

#[test]
fn a_backup_that_cannot_be_built_fails_honestly() {
  let scene = takeover_into_unbuildable();
  let output = run(&scene.config, &scene.workspace, "go");
  let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
  assert!(
    !output.status.success(),
    "nothing served this turn: {stderr}"
  );
  // The takeover is still recorded: the runtime did decide to move, and the record
  // must not pretend the primary was still in charge.
  assert!(stderr.contains("[failover]"), "{stderr}");
  assert!(stderr.contains("could not be initialized"), "{stderr}");
  assert!(stderr.contains("base_url"), "{stderr}");
  assert!(
    stderr.contains("refused: it is the active model"),
    "a second takeover into the same model must be named: {stderr}"
  );
  assert_eq!(scene.primary.requests().len(), 2);
  assert!(
    scene.standby.requests().is_empty(),
    "an unbuildable backup must not be addressed"
  );
}

#[test]
fn no_backup_means_a_down_primary_is_simply_fatal() {
  let temp = TempDir::new().expect("state root");
  let root = temp.path();
  let primary = FakeServer::answer(vec![unavailable(), unavailable()]);
  let workspace = root.join("workspace");
  fs::create_dir_all(&workspace).expect("workspace");
  let config = write_config(root, &primary.base_url(), Backup::None);
  let output = run(&config, &workspace, "go");
  let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
  assert!(!output.status.success(), "{stderr}");
  assert!(!stderr.contains("[failover]"), "{stderr}");
  assert!(stderr.contains("provider_unavailable"), "{stderr}");
  // Two attempts against the only model available, and no silent third.
  assert_eq!(primary.requests().len(), 2);
  let events = trace(&root.join("state"));
  assert!(
    !events
      .iter()
      .any(|event| event.kind == "model_failover" || event.kind == "model_epoch_started" && false),
    "no backup, no epoch transition"
  );
  assert_eq!(
    events
      .iter()
      .filter(|event| event.kind == "model_epoch_started")
      .count(),
    1,
    "only the initial epoch"
  );
}

#[test]
fn a_committed_tool_result_crosses_the_failover_boundary() {
  // The hardest case in the failover phase: the model asks for a side effect, the
  // side effect commits, and only then does the model go away. The backup must
  // continue from the recorded result, and the primary must not be asked to redo
  // the call.
  //
  // The operator, not the model, grants mutation. Without this the registry refuses
  // the write, which is the correct answer to a different question.
  // The call is served, then the primary is unavailable for the attempt that carried the
  // result *and* for its retry: three answers for three requests, which is the runtime's
  // attempt budget written down. The earlier fixture scripted two and got a third
  // connection refused, which is why the count looked like two.
  let primary = FakeServer::answer(vec![
    tool_call(
      "call_write",
      "write",
      r#"{"path":"note.txt","contents":"from the primary"}"#,
    ),
    unavailable(),
    unavailable(),
  ]);
  let standby = FakeServer::answer(vec![text_response("the note is saved")]);
  let standby_url = standby.base_url();
  let temp = TempDir::new().expect("workspace");
  let root = temp.path().to_path_buf();
  let workspace = root.join("workspace");
  fs::create_dir_all(&workspace).expect("workspace");
  let mut config = config_at(&root, &primary.base_url(), Backup::Served(standby_url));
  config.tools.auto_approve_mutating = true;
  let config = write_config_from(&root, &config);

  let output = run(&config, &workspace, "save a note");
  let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
  let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
  assert!(output.status.success(), "{stderr}");
  assert_eq!(stdout, "the note is saved\n");

  // What each endpoint was asked, taken before the filesystem is consulted: a missing
  // side effect here is almost always a shifted script, and the request bodies are what
  // tell the two apart.
  let primary_asks = primary.requests();
  let standby_asks = standby.requests();

  // The write happened.
  assert_eq!(
    read_written(&workspace.join("note.txt"), &primary_asks),
    b"from the primary"
  );

  // Two requests reached the primary after the tool committed: an attempt and a retry.
  // A deterministic listener shows both, where the earlier fixture showed one: its
  // listener was gone by the time the retry was made, so the retry was refused and never
  // recorded. "Two requests" had been measuring a closed port, not two attempts.
  assert_eq!(
    primary_asks.len(),
    3,
    "the tool-call turn, then an attempt and a retry"
  );
  // And neither of them re-issued the call: every request after the commit carries the
  // committed result, so a primary that came back would have continued from the write.
  assert!(
    !primary_asks[0].body.contains(r#""role":"tool""#),
    "the first request had no result to carry: {}",
    primary_asks[0].body
  );
  for ask in &primary_asks[1..] {
    assert!(
      ask.body.contains(r#""role":"tool""#),
      "a request after the commit must carry the committed result: {}",
      ask.body
    );
  }

  // The backup continued from the committed result rather than from a re-ask.
  let asked = standby_asks
    .into_iter()
    .next()
    .expect("the backup was asked");
  assert!(
    asked.line.contains("POST /v1/chat/completions"),
    "the backup is addressed through its endpoint's own path: {}",
    asked.line
  );
  let served = asked.body;
  assert!(
    served.contains(r#""role":"tool""#),
    "the committed tool result must travel to the backup: {served}"
  );
  assert!(
    served.contains("note.txt"),
    "the result names the file: {served}"
  );

  // And each major event keeps the model that produced it.
  let events = trace(&root.join("state"));
  let issued: Vec<&Recorded> = events
    .iter()
    .filter(|event| {
      event.kind == "tool_requested" && event.call_id.as_deref() == Some("call_write")
    })
    .collect();
  assert_eq!(
    issued.len(),
    1,
    "the model issued this call once, and a replayed write of the same bytes would not \
      have shown up anywhere else"
  );
  let completions: Vec<&Recorded> = events
    .iter()
    .filter(|event| event.kind == "tool_completed")
    .collect();
  assert_eq!(completions.len(), 1, "one completion for one call");
  assert_eq!(
    completions[0].model.as_deref(),
    Some("fake/agent"),
    "the write belongs to the model that asked for it"
  );
  assert_eq!(completions[0].epoch, Some(0));
  let turn = events
    .iter()
    .find(|event| event.kind == "turn_completed")
    .expect("turn completed");
  assert_eq!(turn.model.as_deref(), Some("fake/standby"));
}

#[test]
fn a_backup_without_tool_calling_is_refused_by_name_and_never_asked() {
  // The capability gate: a backup that cannot do the work in flight is not a rescue,
  // and failing over into it would trade one failure for a quieter second one.
  // Two things must be true of the refusal. Nothing may be asked of that backup, and
  // the reason must be said: an abstention that looks like "no backup was configured"
  // leaves the operator with a backup they set up and never used, for reasons they
  // cannot see.
  // An empty script is the assertion's other half: the shared harness reports a scripted
  // answer nobody asked for, so nothing here pretends the standby had something to say.
  let scene = takeover_with_narrow_backup(Vec::new(), |caps| {
    caps.tools = false;
  });
  let output = run(&scene.config, &scene.workspace, "go");
  let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
  let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
  assert!(
    !output.status.success(),
    "nothing served this turn: {stderr}"
  );
  assert_eq!(stdout, "", "a refusal does not produce an answer: {stdout}");
  assert!(
    stderr.contains("fake/standby") && stderr.contains("tool calling"),
    "the refusal names the backup and the missing capability: {stderr}"
  );
  assert!(
    !stderr.contains("[failover]"),
    "a refusal is not a takeover and must not be rendered as one: {stderr}"
  );
  // The primary spent its attempt budget, and the standby was never addressed: the
  // refusal happens in the policy, so the deferred adapter is never even built.
  assert_eq!(scene.primary.requests().len(), 2);
  assert!(
    scene.standby.requests().is_empty(),
    "a refused backup must not be asked, nor built, nor paid for"
  );
  let events = trace(&scene.state);
  assert_eq!(
    events
      .iter()
      .filter(|event| event.kind == "model_failover")
      .count(),
    0,
    "no takeover may be recorded: {stderr}"
  );
  assert_eq!(
    events
      .iter()
      .filter(|event| event.kind == "model_epoch_started")
      .count(),
    1,
    "only the epoch that was already active"
  );
}

#[test]
fn a_smaller_backup_takes_over_and_names_the_window_it_lost() {
  // A narrower window is a cost, not a disqualification: compaction can close it. The
  // switch therefore happens, and the transcript says what it cost.
  let scene =
    takeover_with_narrow_backup(vec![text_response("served by the small standby")], |caps| {
      caps.context_window = 1_024;
    });
  let output = run(&scene.config, &scene.workspace, "go");
  let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
  let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
  assert!(output.status.success(), "{stderr}");
  assert_eq!(stdout, "served by the small standby\n");
  assert!(stderr.contains("[failover]"), "{stderr}");
  assert!(
    stderr.contains("context window 1024 < required 32768"),
    "the cost of the switch is on the line: {stderr}"
  );
  // And only what actually happened is claimed. One turn was in flight, so there was
  // no older history to shorten: a `[failover] ... context rebudgeted` here would be
  // a recorded reduction that never took place.
  assert!(
    !stderr.contains("context rebudgeted"),
    "nothing was dropped, so nothing may claim it was: {stderr}"
  );
  let events = trace(&scene.state);
  let failovers: Vec<&Recorded> = events
    .iter()
    .filter(|event| event.kind == "model_failover")
    .collect();
  assert_eq!(failovers.len(), 1);
  assert_eq!(
    events
      .iter()
      .filter(|event| event.kind == "context_reduced")
      .count(),
    0,
    "a takeover that dropped nothing records no reduction"
  );
}
