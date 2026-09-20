//! `rupi packages` as a process: listing and inspection on stdout, diagnostics on stderr.

use std::{fs, path::Path, process::Command};

use tempfile::TempDir;

fn binary() -> &'static str {
  env!("CARGO_BIN_EXE_rupi")
}

fn fixture_home() -> &'static str {
  concat!(env!("CARGO_MANIFEST_DIR"), "/tests/compat/packages/home")
}

fn fixture_project() -> &'static str {
  concat!(env!("CARGO_MANIFEST_DIR"), "/tests/compat/packages/project")
}

fn run(args: &[&str], home: &str, cwd: &str) -> (String, String, bool) {
  let output = Command::new(binary())
    .args(args)
    .env("HOME", home)
    .env("USERPROFILE", home)
    .current_dir(Path::new(cwd))
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
fn packages_lists_global_packages_without_project_flag() {
  let (stdout, stderr, success) = run(&["packages"], fixture_home(), fixture_project());
  assert!(success, "packages command should succeed");
  assert!(stdout.contains("global  fixture-pkg-a  1.0.0"));
  assert!(stdout.contains("First fixture package with skills and prompts."));
  assert!(stdout.contains("global  fixture-pkg-ext  0.5.0"));
  // project package is not in stdout
  assert!(!stdout.contains("project-pkg"));
  // stderr reports project locations skipped
  assert!(stderr.contains("project locations were not read; pass --project to read them"));
}

#[test]
fn packages_with_project_flag_lists_both_global_and_project() {
  let (stdout, stderr, success) = run(
    &["packages", "--project"],
    fixture_home(),
    fixture_project(),
  );
  assert!(success, "packages command should succeed");
  assert!(stdout.contains("global  fixture-pkg-a  1.0.0"));
  assert!(stdout.contains("project  project-pkg  2.1.0"));
  assert!(stdout.contains("Project-local package."));
  assert!(!stderr.contains("project locations were not read"));
}

#[test]
fn packages_show_displays_detailed_surfaces_and_locations() {
  let (stdout, stderr, success) = run(
    &["packages", "--show", "fixture-pkg-a"],
    fixture_home(),
    fixture_project(),
  );
  assert!(success, "packages --show should succeed");
  assert!(stdout.contains("Package: fixture-pkg-a (1.0.0) [global]"));
  assert!(stdout.contains("Surfaces:"));
  assert!(stdout.contains("✓ skills"));
  assert!(stdout.contains("✓ prompts"));
  assert!(stdout.contains("Skills:"));
  assert!(stdout.contains("Prompts:"));
  // stderr should not have missing package error
  assert!(!stderr.contains("no package named"));
}

#[test]
fn packages_show_displays_partial_extensions() {
  let (stdout, _stderr, success) = run(
    &["packages", "--show", "fixture-pkg-ext"],
    fixture_home(),
    fixture_project(),
  );
  assert!(success, "packages --show should succeed");
  assert!(stdout.contains("Package: fixture-pkg-ext (0.5.0) [global]"));
  assert!(stdout.contains("△ extensions"));
  assert!(stdout.contains("Diagnostics:"));
  assert!(stdout.contains("extension surface requires explicit trusted Node host activation"));
}

#[test]
fn packages_show_unknown_package_exits_nonzero() {
  let (_stdout, stderr, success) = run(
    &["packages", "--show", "does-not-exist"],
    fixture_home(),
    fixture_project(),
  );
  assert!(!success, "packages --show for nonexistent should fail");
  assert!(stderr.contains("no package named 'does-not-exist'"));
}

#[test]
fn packages_help_flag_displays_help() {
  let (stdout, _stderr, success) = run(&["packages", "--help"], fixture_home(), fixture_project());
  assert!(success);
  assert!(stdout.contains("Usage: rupi packages"));
  assert!(stdout.contains("packages install"));
}

#[test]
fn packages_install_copies_a_local_package_to_global_home() {
  let temp = TempDir::new().expect("tempdir");
  let home = temp.path().join("home");
  let cwd = temp.path().join("cwd");
  let source = temp.path().join("source");
  fs::create_dir_all(source.join("skills")).expect("create source");
  fs::create_dir_all(&cwd).expect("create cwd");
  fs::write(
    source.join("package.json"),
    r#"{"name":"cli-local","version":"1.0.0","pi":{"skills":["skills"]}}"#,
  )
  .expect("write manifest");
  fs::write(
    source.join("skills/SKILL.md"),
    "---\nname: local\ndescription: local\n---\nbody\n",
  )
  .expect("write skill");
  let source_text = source.to_str().expect("utf8 source");
  let (stdout, stderr, success) = run(
    &["packages", "install", source_text],
    home.to_str().unwrap(),
    cwd.to_str().unwrap(),
  );
  assert!(success, "stdout={stdout}\nstderr={stderr}");
  assert!(stdout.contains("installed cli-local"), "{stdout}");
  assert!(
    home
      .join(".pi/agent/packages/cli-local/package.json")
      .is_file()
  );
  assert!(
    home
      .join(".pi/agent/packages/cli-local/skills/SKILL.md")
      .is_file()
  );
}

#[test]
fn packages_install_rejects_remote_sources_explicitly() {
  let temp = TempDir::new().expect("tempdir");
  let cwd = temp.path().join("cwd");
  fs::create_dir_all(&cwd).expect("create cwd");
  let (stdout, stderr, success) = run(
    &["packages", "install", "npm:example"],
    temp.path().join("home").to_str().unwrap(),
    cwd.to_str().unwrap(),
  );
  assert!(!success, "stdout={stdout}\nstderr={stderr}");
  assert!(stderr.contains("remote package sources"), "{stderr}");
}
