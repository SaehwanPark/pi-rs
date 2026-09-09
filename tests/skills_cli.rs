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

#[test]
fn the_control_prompt_replaces_the_listing_when_asked_for() {
  let home = fixture_home();
  let temp = tempfile::TempDir::new().expect("temp project");
  let (stdout, _stderr) = skills(&["skills", "--control-prompt"], &home, temp.path());
  assert!(stdout.contains("<available_skills>"), "{stdout}");
  assert!(stdout.contains("<name>pdf-tools</name>"), "{stdout}");
  // The fixture's one disable-model-invocation skill: listed by `pi-rs skills`, never
  // offered to a model.
  assert!(
    !stdout.contains("loose"),
    "the control prompt must not name explicit-only skills:\n{stdout}"
  );
}

#[test]
fn show_prints_one_body_and_reaches_even_the_skill_the_model_may_not() {
  let home = fixture_home();
  let temp = tempfile::TempDir::new().expect("temp project");
  let (stdout, _stderr) = skills(&["skills", "--show", "pdf-tools"], &home, temp.path());
  assert!(
    !stdout.contains("---"),
    "frontmatter is not the body:\n{stdout}"
  );
  assert!(
    stdout.contains("references/api.md"),
    "the body, and nothing before it:\n{stdout}"
  );

  // `loose` is exactly the skill the model may not reach for; asking for it by name is
  // the explicit invocation its flag reserves for the user, so it must not be refused.
  let (_stdout, stderr) = skills(&["skills", "--show", "loose"], &home, temp.path());
  assert!(
    !stderr.contains("no skill named"),
    "an explicit-only skill is showable on request:\n{stderr}"
  );

  let (stdout, stderr) = skills(&["skills", "--show", "no-such-skill"], &home, temp.path());
  assert!(stdout.is_empty(), "{stdout}");
  assert!(stderr.contains("no skill named"), "{stderr}");
}
