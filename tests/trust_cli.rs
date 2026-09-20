//! Explicit trust decisions are durable CLI state, not a side effect of reading files.

use std::{fs, process::Command};

use tempfile::TempDir;

fn binary() -> &'static str {
  env!("CARGO_BIN_EXE_rupi")
}

fn run(args: &[&str]) -> (String, String, bool) {
  let output = Command::new(binary())
    .args(args)
    .env("NO_COLOR", "1")
    .output()
    .expect("run rupi");
  (
    String::from_utf8_lossy(&output.stdout).into_owned(),
    String::from_utf8_lossy(&output.stderr).into_owned(),
    output.status.success(),
  )
}

#[test]
fn trust_grant_deny_clear_and_list_use_canonical_project_scope() {
  let temp = TempDir::new().expect("tempdir");
  let store = temp.path().join("state");
  let project = temp.path().join("repo");
  let nested = project.join("src");
  fs::create_dir_all(&nested).expect("create project");
  fs::create_dir_all(project.join(".git")).expect("create git marker");
  let store_text = store.to_str().expect("utf8 store");
  let nested_text = nested.to_str().expect("utf8 nested");

  let (stdout, stderr, success) = run(&[
    "trust",
    "--store",
    store_text,
    "--project",
    nested_text,
    "--grant",
  ]);
  assert!(success, "stdout={stdout}\nstderr={stderr}");
  assert!(stdout.contains("trusted project:"), "{stdout}");
  assert!(store.join("trust.json").is_file());

  let (stdout, stderr, success) = run(&["trust", "--store", store_text, "--list"]);
  assert!(success, "stdout={stdout}\nstderr={stderr}");
  assert!(
    stdout.contains("project:") && stdout.contains("trusted"),
    "{stdout}"
  );

  let (stdout, stderr, success) = run(&[
    "trust",
    "--store",
    store_text,
    "--project",
    nested_text,
    "--deny",
  ]);
  assert!(success, "stdout={stdout}\nstderr={stderr}");
  assert!(stdout.contains("denied project:"), "{stdout}");

  let (stdout, stderr, success) = run(&[
    "trust",
    "--store",
    store_text,
    "--project",
    project.to_str().unwrap(),
    "--clear",
  ]);
  assert!(success, "stdout={stdout}\nstderr={stderr}");
  assert!(stdout.contains("cleared project:"), "{stdout}");

  let (_stdout, stderr, success) = run(&[
    "trust",
    "--store",
    store_text,
    "--project",
    "/does/not/exist",
    "--grant",
  ]);
  assert!(!success);
  assert!(stderr.contains("cannot resolve project"), "{stderr}");
}

#[test]
fn trust_list_does_not_require_a_project_or_provider() {
  let temp = TempDir::new().expect("tempdir");
  let store = temp.path().join("state");
  let (stdout, stderr, success) = run(&["trust", "--store", store.to_str().unwrap(), "--list"]);
  assert!(success, "stdout={stdout}\nstderr={stderr}");
  assert!(stdout.is_empty());
  assert!(stderr.is_empty());
}
