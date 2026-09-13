//! `pi-rs packages` as a process: listing and inspection on stdout, diagnostics on stderr.

use std::{path::Path, process::Command};

fn binary() -> &'static str {
  env!("CARGO_BIN_EXE_pi-rs")
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
    .expect("run pi-rs");
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
fn packages_show_displays_unsupported_extensions() {
  let (stdout, _stderr, success) = run(
    &["packages", "--show", "fixture-pkg-ext"],
    fixture_home(),
    fixture_project(),
  );
  assert!(success, "packages --show should succeed");
  assert!(stdout.contains("Package: fixture-pkg-ext (0.5.0) [global]"));
  assert!(stdout.contains("✗ extensions"));
  assert!(stdout.contains("Diagnostics:"));
  assert!(stdout.contains("unsupported surface 'extensions'"));
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
  assert!(stdout.contains("Usage: pi-rs packages"));
}
