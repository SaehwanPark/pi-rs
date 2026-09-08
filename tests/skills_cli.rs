//! `pi-rs skills` as a process: what belongs on stdout, and what does not.
//!
//! The scan itself is covered by `tests/compat_skills.rs`. What only a process can show
//! is the partition: the listing is the answer, so it goes to stdout and nothing else
//! does, because `pi-rs skills | fzf` is the way this command actually gets used.

use std::{
  path::{Path, PathBuf},
  process::Command,
};

fn binary() -> &'static str {
  env!("CARGO_BIN_EXE_pi-rs")
}

/// Scan the committed fixture tree as `$HOME`, from `cwd`.
fn skills(args: &[&str], home: &Path, cwd: &Path) -> (String, String) {
  let output = Command::new(binary())
    .args(args)
    .env("HOME", home)
    .env("USERPROFILE", home)
    .current_dir(cwd)
    .env("NO_COLOR", "1")
    .output()
    .expect("run pi-rs");
  (
    String::from_utf8_lossy(&output.stdout).into_owned(),
    String::from_utf8_lossy(&output.stderr).into_owned(),
  )
}

fn fixture_home() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("tests/compat/skills/home")
    .canonicalize()
    .expect("fixture home")
}

#[test]
fn stdout_is_the_listing_and_nothing_else() {
  let home = fixture_home();
  let temp = tempfile::TempDir::new().expect("temp project");
  let (stdout, stderr) = skills(&["skills"], &home, temp.path());
  for name in ["dup", "loose", "pdf-tools", "team-notes"] {
    assert!(stdout.contains(name), "{stdout}");
  }
  assert!(
    !stdout.contains("[skill]"),
    "a decision about a skipped file is not part of the listing: {stdout}"
  );
  assert!(stderr.contains("no description"), "{stderr}");
  assert!(
    stderr.contains("project locations were not read"),
    "an empty-looking result that came from refusing to look has to say so: {stderr}"
  );
}

#[test]
fn a_project_skill_appears_only_when_the_project_was_named() {
  let home = fixture_home();
  let temp = tempfile::TempDir::new().expect("temp project");
  let skill = temp.path().join(".agents/skills/repo-helper");
  std::fs::create_dir_all(&skill).expect("create skill dir");
  std::fs::write(
    skill.join("SKILL.md"),
    "---\nname: repo-helper\ndescription: Helps in this repo.\n---\n",
  )
  .expect("write skill");

  let (untrusted, _) = skills(&["skills"], &home, temp.path());
  assert!(!untrusted.contains("repo-helper"), "{untrusted}");
  let (trusted, stderr) = skills(&["skills", "--project"], &home, temp.path());
  assert!(trusted.contains("project  repo-helper"), "{trusted}");
  assert!(
    !stderr.contains("project locations were not read"),
    "the gate was opened, so the notice must stop: {stderr}"
  );
}

#[test]
fn a_mistyped_flag_fails_loudly_rather_than_reading_more_than_asked() {
  let home = fixture_home();
  let temp = tempfile::TempDir::new().expect("temp project");
  let (stdout, stderr) = skills(&["skills", "--trust"], &home, temp.path());
  assert!(stdout.is_empty(), "{stdout}");
  assert!(
    stderr.contains("unknown skills argument '--trust'"),
    "{stderr}"
  );
}
