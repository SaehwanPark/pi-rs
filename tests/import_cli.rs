//! End-to-end tests for `pi-rs import-pi`.
//!
//! These run the real binary over the committed Pi fixture, because the command's contract
//! is a stream contract: the report is stdout, the destination line is stderr, and a dry run
//! must not create a byte on disk. The store-level fidelity of the mapping is covered by
//! `crates/pi-rs-store/tests/pi_import.rs`; what is under test here is what a user gets.

use std::{
  fs,
  path::{Path, PathBuf},
  process::Output,
};

use pi_rs_core::{ModelCapabilities, ModelEndpoint, ModelRef, ReasoningExposure, RuntimeConfig};
use pi_rs_store::StateLayout;
use tempfile::TempDir;

const FIXTURE: &str = concat!(
  env!("CARGO_MANIFEST_DIR"),
  "/crates/pi-rs-store/tests/fixtures/pi/branched.jsonl"
);
const SESSION_ID: &str = "pi-a1b2c3d4-0000-7000-8000-000000000001";

fn import(args: &[&str]) -> Output {
  let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_pi-rs"));
  command
    .arg("import-pi")
    .args(args)
    .env_remove("NO_COLOR")
    .env_remove("TERM")
    .env_remove("CLICOLOR")
    .env_remove("CLICOLOR_FORCE");
  command.output().expect("run pi-rs import-pi")
}

fn text(bytes: &[u8]) -> String {
  String::from_utf8_lossy(bytes).into_owned()
}

/// A config whose `state_dir` is `state` under `root`, so `trace` reads what an import wrote.
fn write_config(root: &Path) -> PathBuf {
  let state = root.join("state");
  let mut config = RuntimeConfig::new(ModelRef::new("fake", "agent"), state.to_string_lossy());
  config.endpoints.push(ModelEndpoint {
    provider: "fake".into(),
    model: "agent".into(),
    base_url: None,
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
    serde_json::to_vec_pretty(&config).expect("config serialises"),
  )
  .expect("write");
  path
}

#[test]
fn a_dry_run_reports_on_stdout_and_writes_nothing() {
  let root = TempDir::new().expect("temp root");
  let state = root.path().join("state");
  let output = import(&[FIXTURE, "--store", state.to_str().expect("utf-8 path")]);
  assert!(output.status.success(), "{}", text(&output.stderr));

  let stdout = text(&output.stdout);
  assert!(stdout.contains("Pi session"), "{stdout}");
  assert!(stdout.contains("version 3"), "{stdout}");
  assert!(stdout.contains("/home/dev/app"), "{stdout}");
  // The kinds that were carried, and the kinds that were deliberately not.
  for expected in [
    "message:user",
    "message:assistant",
    "message:toolResult",
    "left out",
    "compaction",
    "label",
  ] {
    assert!(stdout.contains(expected), "missing '{expected}':\n{stdout}");
  }
  // The sibling branch is counted, not silently folded into the mainline.
  assert!(stdout.contains("off the active path"), "{stdout}");
  assert!(
    stdout.contains(SESSION_ID),
    "the report must name the session id it would file: \n{stdout}"
  );
  assert!(
    text(&output.stderr).contains("nothing written"),
    "{}",
    text(&output.stderr)
  );
  assert!(
    !state.exists(),
    "a dry run named a destination and still created it"
  );
}

#[test]
fn write_files_a_session_the_store_lists_and_trace_reads() {
  let root = TempDir::new().expect("temp root");
  let config = write_config(root.path());
  let state = root.path().join("state");
  let output = import(&[
    FIXTURE,
    "--config",
    config.to_str().expect("utf-8 config"),
    "--write",
  ]);
  assert!(output.status.success(), "{}", text(&output.stderr));
  let stderr = text(&output.stderr);
  assert!(stderr.contains("written under"), "{stderr}");
  assert!(stderr.contains(SESSION_ID), "{stderr}");

  let layout = StateLayout::new(&state);
  let session_id = pi_rs_core::SessionId::from_string(SESSION_ID);
  assert!(
    layout.session_path(&session_id).exists(),
    "no session file at {}",
    layout.session_path(&session_id).display()
  );
  let journal = fs::read_to_string(layout.trace_path(&session_id)).expect("trace journal exists");
  // Content fidelity through the CLI: what Pi's file said, in the journal pi-rs reads back.
  assert!(journal.contains("which files changed?"), "{journal}");
  assert!(journal.contains("Checking the working tree."), "{journal}");
  assert!(journal.contains("provider_summary"), "{journal}");
  // The tool's output has to be in the session somewhere: a blob when the config's inline
  // threshold is smaller than it, journal bytes when it is not.
  let blobs = layout.blobs_dir(&session_id);
  let blob_count = fs::read_dir(&blobs)
    .map(|dir| dir.count())
    .unwrap_or_default();
  assert!(
    blob_count >= 1 || journal.contains("src/lib.rs"),
    "the imported tool output is neither under {} nor in the journal",
    blobs.display()
  );
}

#[test]
fn an_imported_session_reads_back_through_trace() {
  // The point of the import is a session pi-rs can keep working in, so the proof is that
  // the other read command — which knows nothing about Pi — renders it.
  let root = TempDir::new().expect("temp root");
  let config = write_config(root.path());
  let imported = import(&[
    FIXTURE,
    "--config",
    config.to_str().expect("utf-8 config"),
    "--write",
  ]);
  assert!(imported.status.success(), "{}", text(&imported.stderr));

  let output = std::process::Command::new(env!("CARGO_BIN_EXE_pi-rs"))
    .args(["trace", "--config"])
    .arg(&config)
    .env_remove("NO_COLOR")
    .env_remove("TERM")
    .env_remove("CLICOLOR")
    .env_remove("CLICOLOR_FORCE")
    .output()
    .expect("run pi-rs trace");
  assert!(output.status.success(), "{}", text(&output.stderr));
  let stdout = text(&output.stdout);
  assert!(stdout.contains("which files changed?"), "{stdout}");
  assert!(stdout.contains("Checking the working tree."), "{stdout}");
  // Which session was read is metadata about the read, so it belongs on stderr.
  assert!(
    text(&output.stderr).contains(SESSION_ID),
    "{}",
    text(&output.stderr)
  );
  // Pi stored reasoning for one request only, and an import may not invent a provenance
  // for the others.
  assert_eq!(stdout.matches("[provider summary]").count(), 1, "{stdout}");
  assert!(!stdout.contains("declared rationale"), "{stdout}");
}

#[test]
fn importing_the_same_file_twice_is_refused_not_duplicated() {
  let root = TempDir::new().expect("temp root");
  let state = root.path().join("state");
  let store = state.to_str().expect("utf-8 path");
  let first = import(&[FIXTURE, "--store", store, "--write"]);
  assert!(first.status.success(), "{}", text(&first.stderr));
  let second = import(&[FIXTURE, "--store", store, "--write"]);
  assert!(
    !second.status.success(),
    "the second import reported success:\n{}",
    text(&second.stderr)
  );
  let stderr = text(&second.stderr);
  assert!(stderr.contains(SESSION_ID), "{stderr}");
  // The refusal happens after the report, so the reader still sees what was attempted.
  assert!(text(&second.stdout).contains("Pi session"));
}

#[test]
fn a_missing_file_is_named_and_exits_non_zero() {
  let output = import(&["/nonexistent/pi-session.jsonl"]);
  assert_eq!(output.status.code(), Some(1), "{}", text(&output.stderr));
  assert!(
    text(&output.stderr).contains("/nonexistent/pi-session.jsonl"),
    "{}",
    text(&output.stderr)
  );
  assert!(output.stdout.is_empty(), "{}", text(&output.stdout));
}

#[test]
fn usage_errors_exit_two_and_help_exits_zero() {
  let no_file = import(&[]);
  assert_eq!(no_file.status.code(), Some(2));
  assert!(text(&no_file.stderr).contains("Pi session file is required"));

  let no_destination = import(&[FIXTURE, "--write"]);
  assert_eq!(no_destination.status.code(), Some(2));
  assert!(
    text(&no_destination.stderr).contains("--write needs --store"),
    "{}",
    text(&no_destination.stderr)
  );

  let help = import(&["--help"]);
  assert!(help.status.success(), "{}", text(&help.stderr));
  assert!(text(&help.stdout).contains("pi-rs import-pi <pi-session.jsonl>"));
}

#[test]
fn an_unwritable_destination_fails_once_without_a_panic() {
  // A store root underneath a regular file cannot be created by any user. The report has
  // already been printed by then, so the failure is one line on stderr and a non-zero exit.
  let root = TempDir::new().expect("temp root");
  let file = root.path().join("not-a-directory");
  fs::write(&file, b"x").expect("write blocker");
  let store = file.join("state");
  let output = import(&[
    FIXTURE,
    "--store",
    store.to_str().expect("utf-8 path"),
    "--write",
  ]);
  assert!(!output.status.success());
  let stderr = text(&output.stderr);
  assert!(stderr.starts_with("error:"), "{stderr}");
  assert!(!stderr.contains("panicked"), "{stderr}");
  assert!(text(&output.stdout).contains("Pi session"));
}
