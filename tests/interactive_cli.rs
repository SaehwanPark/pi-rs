//! End-to-end tests for `pi-rs interactive`, run where no terminal is attached.
//!
//! The loop itself is verified by the unit tests in `src/interactive.rs`, which is
//! where its decisions live precisely so that they can be. What these tests cover is
//! the part only the built binary has: the arguments, and what happens when the
//! command is started somewhere it cannot draw. A test that drove the loop would
//! need a terminal, and a suite that needs a terminal cannot run in CI, so the
//! boundary itself is what is pinned here — including that a piped invocation
//! *returns*, rather than sitting in raw mode waiting for a key nobody will press.

use std::{
  path::Path,
  process::{Command, Output},
};

use tempfile::TempDir;

fn interactive(config: &Path, cwd: &Path) -> Output {
  Command::new(env!("CARGO_BIN_EXE_pi-rs"))
    .args(["interactive", "--config"])
    .arg(config)
    .arg("--cwd")
    .arg(cwd)
    .output()
    .expect("run pi-rs")
}

#[test]
fn interactive_help_exits_zero_without_any_argument() {
  let output = Command::new(env!("CARGO_BIN_EXE_pi-rs"))
    .args(["interactive", "--help"])
    .output()
    .expect("run pi-rs");
  assert!(output.status.success(), "{output:?}");
  let stdout = String::from_utf8_lossy(&output.stdout);
  assert!(stdout.contains("Usage: pi-rs interactive"), "{stdout}");
  // The two facts a user needs before touching a keyboard: what quits, and that a
  // typed draft survives it.
  assert!(stdout.contains("ctrl-c"), "{stdout}");
  assert!(stdout.contains("never discarded"), "{stdout}");
}

#[test]
fn top_level_help_names_the_interactive_command() {
  let output = Command::new(env!("CARGO_BIN_EXE_pi-rs"))
    .output()
    .expect("run pi-rs");
  assert!(output.status.success(), "{output:?}");
  assert!(
    String::from_utf8_lossy(&output.stdout).contains("pi-rs interactive"),
    "top-level help should list the command"
  );
}

#[test]
fn a_missing_path_is_an_argument_error() {
  let output = Command::new(env!("CARGO_BIN_EXE_pi-rs"))
    .arg("interactive")
    .output()
    .expect("run pi-rs");
  assert_eq!(output.status.code(), Some(2), "{output:?}");
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.starts_with("error: --config is required"),
    "{stderr}"
  );
  // The command's own help, so the reader sees the flag they left out in the
  // context of the command they typed.
  assert!(stderr.contains("Usage: pi-rs interactive"), "{stderr}");
}

/// `stdout` is a pipe for every test in this file, because `Output` captures it.
#[test]
fn a_terminal_is_required_and_asking_for_one_does_not_open_a_session() {
  let temp = TempDir::new().unwrap();
  // Deliberately not a readable config: the terminal is checked first, so this
  // invocation never gets as far as complaining about the file. A bad config is a
  // `pi-rs run` shape of mistake, and the interactive command must not leave the
  // terminal in raw mode on the way to reporting one.
  let missing = temp.path().join("config.json");
  let output = interactive(&missing, temp.path());
  assert_eq!(output.status.code(), Some(1), "{output:?}");
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(stderr.contains("needs a terminal on stdout"), "{stderr}");
  assert!(stderr.contains("pi-rs run"), "{stderr}");
  // Nothing was drawn, because nothing could be.
  assert!(
    output.stdout.is_empty(),
    "{}",
    String::from_utf8_lossy(&output.stdout)
  );
}
