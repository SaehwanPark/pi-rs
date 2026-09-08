//! End-to-end tests for `pi-rs export`.
//!
//! These drive the real binary, because the command's contract is a stream contract: the
//! JSONL is stdout, the dropped-detail report is stderr, and a refused selection must leave
//! the store and the requested path untouched. The mapping itself is covered by the unit
//! tests in `src/export_pi.rs`; what is under test here is what a user gets.
//!
//! The round-trip test asserts content and ordering, never byte equality: an export mints
//! fresh entry ids, so the file it writes is deliberately not the file it read.

use std::{
  fs,
  path::{Path, PathBuf},
  process::Output,
};

use pi_rs_core::{ModelCapabilities, ModelEndpoint, ModelRef, ReasoningExposure, RuntimeConfig};
use tempfile::TempDir;

/// The id the importer gives the fixture below, because it namespaces Pi ids.
const SESSION_ID: &str = "pi-a1b2c3d4-0000-7000-8000-000000000001";
/// What a plain conversation must still say after export, re-import, and export again.
const TURNS: [(&str, &str); 4] = [
  ("user", "which files changed?"),
  ("assistant", "Checking the working tree."),
  ("user", "and this error?"),
  ("assistant", "It compiles."),
];

/// A Pi session file in the shape `import_pi` accepts: header, then one message per turn.
/// `thinking` is the reasoning content of the first assistant entry, empty for a plain turn.
fn pi_session(id: &str, thinking: &str) -> String {
  let reasoning = if thinking.is_empty() {
    String::new()
  } else {
    format!("{{\"type\":\"thinking\",\"thinking\":\"{thinking}\"}},")
  };
  format!(
    concat!(
      "{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2026-07-20T09:00:00.000Z\",\"cwd\":\"/home/dev/app\"}}\n",
      "{{\"type\":\"message\",\"id\":\"e1\",\"parentId\":null,\"timestamp\":\"2026-07-20T09:00:10.000Z\",\"message\":{{\"role\":\"user\",\"content\":\"which files changed?\"}}}}\n",
      "{{\"type\":\"message\",\"id\":\"e2\",\"parentId\":\"e1\",\"timestamp\":\"2026-07-20T09:00:12.000Z\",\"message\":{{\"role\":\"assistant\",\"content\":[{reasoning}{{\"type\":\"text\",\"text\":\"Checking the working tree.\"}}],\"provider\":\"anthropic\",\"model\":\"claude-opus-4-8\"}}}}\n",
      "{{\"type\":\"message\",\"id\":\"e3\",\"parentId\":\"e2\",\"timestamp\":\"2026-07-20T09:00:20.000Z\",\"message\":{{\"role\":\"user\",\"content\":\"and this error?\"}}}}\n",
      "{{\"type\":\"message\",\"id\":\"e4\",\"parentId\":\"e3\",\"timestamp\":\"2026-07-20T09:00:22.000Z\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"It compiles.\"}}]}}}}\n"
    ),
    id = id,
    reasoning = reasoning,
  )
}

/// A config whose `state_dir` is `state` under `root`, so export reads what import wrote.
fn write_config(root: &Path, state: &Path) -> PathBuf {
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
  let path = root.join(format!(
    "config-{}.json",
    state.file_name().unwrap().to_string_lossy()
  ));
  fs::write(
    &path,
    serde_json::to_vec_pretty(&config).expect("config serialises"),
  )
  .expect("write");
  path
}

fn export(config: &Path, args: &[&str]) -> Output {
  std::process::Command::new(env!("CARGO_BIN_EXE_pi-rs"))
    .args(["export"])
    .args(args)
    .args(["--config"])
    .arg(config)
    .env_remove("NO_COLOR")
    .output()
    .expect("run pi-rs export")
}

/// Imports `file` as a real session under `config`'s state root.
fn import(file: &Path, config: &Path) -> Output {
  std::process::Command::new(env!("CARGO_BIN_EXE_pi-rs"))
    .args(["import-pi"])
    .arg(file)
    .args(["--write", "--config"])
    .arg(config)
    .env_remove("NO_COLOR")
    .output()
    .expect("run pi-rs import-pi")
}

fn stdout(out: &Output) -> String {
  String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
  String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The (role, text) of every message entry, in the order the file carries them.
fn turns(jsonl: &str) -> Vec<(String, String)> {
  jsonl
    .lines()
    .filter(|line| !line.trim().is_empty())
    .map(|line| -> serde_json::Value {
      serde_json::from_str(line).expect("one JSON object per line")
    })
    .filter(|entry| entry["type"] == "message")
    .map(|entry| {
      let message = &entry["message"];
      let text = match &message["content"] {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(blocks) => blocks
          .iter()
          .filter(|block| block["type"] == "text")
          .filter_map(|block| block["text"].as_str())
          .collect::<Vec<&str>>()
          .join(""),
        _ => String::new(),
      };
      (message["role"].as_str().expect("a role").to_owned(), text)
    })
    .collect()
}

fn expected_turns() -> Vec<(String, String)> {
  TURNS
    .iter()
    .map(|(role, text)| (role.to_string(), text.to_string()))
    .collect()
}

/// The session id an import reports it wrote, so a test never hardcodes namespacing rules.
fn written_session_id(report: &str) -> String {
  let line = report
    .lines()
    .find(|line| line.contains("session ") && line.contains(" written"))
    .expect("the import names the session it wrote");
  line
    .split("session ")
    .nth(1)
    .expect("a session id")
    .split_whitespace()
    .next()
    .expect("a session id")
    .to_owned()
}

/// Imports the fixture into `state`, then returns a config pointing at that state root.
fn seeded_store(root: &Path, state: &str, fixture: &str, id: &str) -> PathBuf {
  let config = write_config(root, &root.join(state));
  let file = root.join(format!("fixture-{id}.jsonl"));
  fs::write(&file, fixture).expect("write fixture");
  let imported = import(&file, &config);
  assert!(
    imported.status.success(),
    "seed the store: {}",
    stderr(&imported)
  );
  config
}

#[test]
fn an_export_reimports_into_a_fresh_store_with_the_same_turns_in_the_same_order() {
  let dir = TempDir::new().expect("temp dir");
  let source = seeded_store(
    dir.path(),
    "state-source",
    &pi_session("a1b2c3d4-0000-7000-8000-000000000001", ""),
    SESSION_ID,
  );

  let out = export(&source, &[SESSION_ID]);
  assert!(out.status.success(), "{}", stderr(&out));
  let first = stdout(&out);
  let header: serde_json::Value =
    serde_json::from_str(first.lines().next().expect("a header line")).expect("a JSON header");
  assert_eq!(
    header["type"], "session",
    "the first line is the session header"
  );
  assert_eq!(
    turns(&first),
    expected_turns(),
    "an export is the conversation in trace order: {first}"
  );

  // Re-import that export into a store that has never seen it, then export the re-import.
  let exported_file = dir.path().join("exported.jsonl");
  fs::write(&exported_file, &first).expect("write export");
  let target = write_config(dir.path(), &dir.path().join("state-target"));
  let reimported = import(&exported_file, &target);
  assert!(reimported.status.success(), "{}", stderr(&reimported));
  let reimported_id = written_session_id(&stderr(&reimported));

  let again = export(&target, &[&reimported_id]);
  assert!(again.status.success(), "{}", stderr(&again));
  assert_eq!(
    turns(&stdout(&again)),
    turns(&first),
    "the re-imported session holds the same user and assistant content, in order"
  );
}
