//! `pi-rs run --resume`: the flag surface and the paths that refuse to run.
//!
//! These tests drive the binary and check what the command leaves behind, because the
//! contract is about the store as much as about the message: a name that does not match
//! a recorded session must not be able to create one, and a value that looks like a
//! flag must be refused while the command line is still all the command has read.
//!
//! No test here needs a provider. The endpoint points at a port that was bound and
//! released, so any request would fail with a connection error rather than a resume
//! error, which is what makes "the run never reached the provider" visible.

use std::{
  fs,
  net::TcpListener,
  path::{Path, PathBuf},
  process::{Command, Output},
};

use pi_rs_core::{
  ModelCapabilities, ModelEndpoint, ModelRef, ReasoningExposure, RuntimeConfig, SessionId,
};
use pi_rs_store::StateLayout;
use tempfile::TempDir;

/// A session id shape the store would list, used as the name of a session that exists.
const RECORDED: &str = "resume-fixture-0001";

/// An endpoint nobody is listening on.
fn closed_endpoint() -> String {
  let listener = TcpListener::bind("127.0.0.1:0").expect("bind an unused port");
  let address = listener.local_addr().expect("unused port address");
  drop(listener);
  format!("http://{address}/v1")
}

/// Config whose store root is `root/state`, which may or may not exist yet.
fn write_config(root: &Path) -> PathBuf {
  let state = root.join("state");
  let mut config = RuntimeConfig::new(
    ModelRef::new("fake", "agent"),
    state.to_string_lossy().to_string(),
  );
  config.endpoints.push(ModelEndpoint {
    provider: "fake".into(),
    model: "agent".into(),
    base_url: Some(closed_endpoint()),
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
  });
  let path = root.join("config.json");
  fs::write(
    &path,
    serde_json::to_vec_pretty(&config).expect("serialize config"),
  )
  .expect("write config");
  path
}

/// Put one session in the store: the session file is what makes an id listable, and an
/// empty file is a session with no records, which is enough to name.
fn record_session(state: &Path, id: &str) -> PathBuf {
  let layout = StateLayout::new(state);
  fs::create_dir_all(layout.sessions_dir()).expect("create the sessions dir");
  let path = layout.session_path(&SessionId::from_string(id.to_string()));
  fs::write(&path, "").expect("write the session file");
  path
}

fn recorded_count(state: &Path) -> usize {
  StateLayout::new(state)
    .list_session_ids()
    .expect("list sessions")
    .len()
}

fn run(config: &Path, cwd: &Path, extra: &[&str]) -> Output {
  Command::new(env!("CARGO_BIN_EXE_pi-rs"))
    .args(["run", "--config"])
    .arg(config)
    .arg("--cwd")
    .arg(cwd)
    .args(["--prompt", "continue this"])
    .args(extra)
    .output()
    .expect("run pi-rs")
}

fn stderr(out: &Output) -> String {
  String::from_utf8_lossy(&out.stderr).to_string()
}

#[test]
fn a_resume_value_that_looks_like_a_flag_is_refused_before_anything_is_opened() {
  // The config path is deliberately absent: had the parser handed the flag-shaped
  // value to the runtime, the failure would be about reading a config file. Both the
  // spaced and the inline form must be caught while the command line is all that has
  // been read.
  let temp = TempDir::new().expect("temp dir");
  let absent = temp.path().join("absent-config.json");
  for flag in ["--resume", "--resume=-x"] {
    let extra = if flag == "--resume" {
      ["--resume", "--quiet"]
    } else {
      [flag, "ignored"]
    };
    let out = run(&absent, temp.path(), &extra);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    let message = stderr(&out);
    assert!(message.contains("--resume needs a session id"), "{message}");
    assert!(!message.contains("cannot read config"), "{message}");
  }
  // Nothing was opened, so nothing was created: the workspace stays empty.
  assert_eq!(fs::read_dir(temp.path()).expect("read temp dir").count(), 0);
}

#[test]
fn a_resume_value_that_is_absent_is_refused() {
  let temp = TempDir::new().expect("temp dir");
  let absent = temp.path().join("absent-config.json");
  let out = run(&absent, temp.path(), &["--resume"]);
  assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
  assert!(stderr(&out).contains("--resume requires a value"));
}

#[test]
fn an_unknown_session_id_is_refused_without_creating_a_session() {
  let temp = TempDir::new().expect("temp dir");
  let config = write_config(temp.path());
  let state = temp.path().join("state");
  let out = run(&config, temp.path(), &["--resume", "nosuchsession"]);
  assert!(!out.status.success(), "stderr: {}", stderr(&out));
  let message = stderr(&out);
  assert!(
    message.contains("no session id starts with 'nosuchsession'"),
    "{message}"
  );
  // The store root is created by the write path only. A name that matches nothing
  // must not leave a store behind, and must not have reached the endpoint.
  assert!(!state.exists(), "a store was created for an unknown id");
  assert!(!message.contains("connection"), "{message}");
}

#[test]
fn a_recorded_session_is_refused_rather_than_replaced_by_a_fresh_one() {
  // Continuing a session means appending to it. Until the model-visible context of a
  // recorded session is rebuilt from its log, the command refuses: a turn built from
  // the prompt alone would be a new conversation wearing an existing session id.
  let temp = TempDir::new().expect("temp dir");
  let config = write_config(temp.path());
  let state = temp.path().join("state");
  let session = record_session(&state, RECORDED);
  let before = fs::read(&session).expect("read the session file");

  // A prefix names the session, exactly as it does for `pi-rs trace`.
  let out = run(&config, temp.path(), &["--resume", "resume-fixture"]);
  assert!(!out.status.success(), "stderr: {}", stderr(&out));
  let message = stderr(&out);
  assert!(message.contains("cannot continue session"), "{message}");
  assert!(message.contains(RECORDED), "{message}");
  assert!(message.contains("context"), "{message}");

  // No second session, and the named one is untouched: refusal writes nothing.
  assert_eq!(recorded_count(&state), 1);
  assert_eq!(
    fs::read(&session).expect("re-read the session file"),
    before
  );
}
