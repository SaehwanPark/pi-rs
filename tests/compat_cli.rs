//! `pi-rs compat` CLI end-to-end integration tests.

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
fn compat_inspect_package_with_extension() {
  let pkg_path = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/compat/packages/with-extension"
  );
  let (stdout, _stderr, success) = run(&["compat", pkg_path], fixture_home(), fixture_project());
  assert!(success, "compat should succeed on with-extension package");
  assert!(stdout.contains("Compatibility target: Pi 0.50.x+"));
  assert!(stdout.contains("Target: with-extension (package)"));
  assert!(stdout.contains("✓ package manifest (with-extension@1.0.0)"));
  assert!(stdout.contains("✓ skill discovery (1 skill(s) found)"));
  assert!(stdout.contains("✓ prompt templates (1 prompt template(s) found)"));
  assert!(stdout.contains("✓ tool registration (registerTool API detected)"));
  assert!(stdout.contains("✓ command registration (registerCommand API detected)"));
  assert!(stdout.contains("△ context hook (context lifecycle hook detected)"));
  assert!(stdout.contains("✗ unsupported internal import (internal Pi module import detected)"));
  assert!(stdout.contains(
    "△ extensions (selected TypeScript APIs available; explicit host activation required)"
  ));
}

#[test]
fn compat_inspect_package_manifest_file() {
  let manifest_path = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/compat/packages/minimal/package.json"
  );
  let (stdout, _stderr, success) = run(
    &["compat", manifest_path],
    fixture_home(),
    fixture_project(),
  );
  assert!(success, "compat should succeed on package.json file");
  assert!(stdout.contains("Compatibility target: Pi 0.50.x+"));
  assert!(stdout.contains("✓ package manifest (minimal-pkg@1.0.0)"));
  assert!(stdout.contains("- extensions"));
}

#[test]
fn compat_inspect_skill_file() {
  let skill_path = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/compat/skills/home/.agents/skills/pdf-tools/SKILL.md"
  );
  let (stdout, _stderr, success) = run(&["compat", skill_path], fixture_home(), fixture_project());
  assert!(success, "compat should succeed on skill file");
  assert!(stdout.contains("Target: SKILL.md (skill)"));
  assert!(stdout.contains("✓ skill frontmatter (valid)"));
  assert!(stdout.contains("✓ skill name (pdf-tools)"));
  assert!(stdout.contains("✓ skill description"));
  assert!(stdout.contains("✓ skill body"));
}

#[test]
fn compat_inspect_prompt_file() {
  let prompt_path = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/compat/prompts/home/.pi/agent/prompts/review.md"
  );
  let (stdout, _stderr, success) = run(&["compat", prompt_path], fixture_home(), fixture_project());
  assert!(success, "compat should succeed on prompt template");
  assert!(stdout.contains("Target: review.md (prompt)"));
  assert!(stdout.contains("✓ prompt frontmatter (valid)"));
  assert!(stdout.contains("✓ prompt description (declared: Review staged git changes)"));
  assert!(stdout.contains("✓ prompt body"));
  assert!(stdout.contains("✓ parameter substitution (placeholders: $1, $@)"));
}

#[test]
fn compat_inspect_named_package_with_project_flag() {
  let (stdout, _stderr, success) = run(
    &["compat", "--project", "project-pkg"],
    fixture_home(),
    fixture_project(),
  );
  assert!(success, "compat should resolve discovered project package");
  assert!(stdout.contains("Target: project-pkg (package)"));
  assert!(stdout.contains("✓ package manifest (project-pkg@2.1.0)"));
}

#[test]
fn compat_inspect_json_output() {
  let pkg_path = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/compat/packages/with-extension"
  );
  let (stdout, _stderr, success) = run(
    &["compat", "--json", pkg_path],
    fixture_home(),
    fixture_project(),
  );
  assert!(success, "compat --json should succeed");
  let json: serde_json::Value = serde_json::from_str(&stdout).expect("valid json output");
  assert_eq!(json["target"], "with-extension");
  assert_eq!(json["kind"], "package");
  assert_eq!(json["pi_target"], "Pi 0.50.x+");

  let surfaces = json["surfaces"].as_array().expect("surfaces array");
  assert!(
    surfaces
      .iter()
      .any(|s| s["name"] == "tool registration" && s["status"] == "supported")
  );
  assert!(
    surfaces
      .iter()
      .any(|s| s["name"] == "context hook" && s["status"] == "partial")
  );
  assert!(
    surfaces
      .iter()
      .any(|s| s["name"] == "extensions" && s["status"] == "partial")
  );
}

#[test]
fn compat_help_flag() {
  let (stdout, _stderr, success) = run(&["compat", "--help"], fixture_home(), fixture_project());
  assert!(success, "compat --help should succeed");
  assert!(stdout.contains("Usage: pi-rs compat"));
  assert!(stdout.contains("--json"));
  assert!(stdout.contains("--project"));
}

#[test]
fn compat_missing_target_fails_with_helpful_error() {
  let (_stdout, stderr, success) = run(&["compat"], fixture_home(), fixture_project());
  assert!(!success, "compat without target should fail");
  assert!(stderr.contains("compat needs a target to inspect"));
  assert!(stderr.contains("Usage: pi-rs compat"));
}
