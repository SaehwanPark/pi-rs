//! `rupi prompts` and `rupi prompt` as processes: what belongs on stdout, and what
//! does not.
//!
//! The scan and the substitution grammar are covered by unit tests in `rupi-compat`.
//! What only a process can show is the partition -- the listing and the expanded prompt
//! are the answer, so nothing else may share stdout with them -- and that the expansion
//! reaches the terminal with its newlines intact, which is where a `println!` would
//! quietly add one.

use std::{
  fs,
  path::{Path, PathBuf},
  process::Command,
};

fn binary() -> &'static str {
  env!("CARGO_BIN_EXE_rupi")
}

/// A `$HOME` holding three templates, and a project directory holding one more.
fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
  let root = tempfile::TempDir::new().expect("temp root");
  let home = root.path().join("home");
  let project = root.path().join("project");
  let global = home.join(".pi/agent/prompts");
  fs::create_dir_all(&global).expect("global prompts");
  fs::create_dir_all(project.join(".git")).expect("project ceiling");
  fs::write(
    global.join("review.md"),
    "---\ndescription: Review staged git changes\n---\nReview the staged changes.\n",
  )
  .expect("write review");
  fs::write(
    global.join("pr.md"),
    "---\ndescription: Review a pull request\nargument-hint: \"<PR-URL>\"\n---\nLook at $1.\n",
  )
  .expect("write pr");
  fs::write(global.join("plain.md"), "Summarize what changed today.\n").expect("write plain");
  fs::write(
    global.join("unclosed.md"),
    "---\ndescription: never closed\n",
  )
  .expect("write unclosed");
  (root, home, project)
}

fn run(args: &[&str], home: &Path, cwd: &Path) -> (String, String, bool) {
  let output = Command::new(binary())
    .args(args)
    .env("HOME", home)
    .env("USERPROFILE", home)
    .current_dir(cwd)
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
fn the_listing_is_the_answer_and_the_decisions_are_not_on_stdout() {
  let (_root, home, project) = fixture();
  let (stdout, stderr, ok) = run(&["prompts"], &home, &project);
  assert!(ok, "{stderr}");
  assert_eq!(
    stdout,
    "global  plain\nSummarize what changed today. (first line of the template)\n\
     global  pr  <PR-URL>\nReview a pull request\n\
     global  review\nReview staged git changes\n"
  );
  assert!(
    stderr.contains("[prompt] ") && stderr.contains("unclosed.md"),
    "a skipped file is named on stderr: {stderr}"
  );
  assert!(
    stderr.contains("project locations were not read"),
    "an empty-looking listing that came from refusing to look has to say so: {stderr}"
  );
}

#[test]
fn a_project_template_appears_only_when_the_project_was_named() {
  let (_root, home, project) = fixture();
  fs::create_dir_all(project.join(".pi/prompts")).expect("project prompts");
  fs::write(
    project.join(".pi/prompts/deploy.md"),
    "---\ndescription: Deploy this project\n---\nDeploy.\n",
  )
  .expect("write deploy");

  let (untrusted, stderr, _) = run(&["prompts"], &home, &project);
  assert!(!untrusted.contains("deploy"), "{untrusted}");
  assert!(stderr.contains("--project"), "{stderr}");

  let (trusted, _, _) = run(&["prompts", "--project"], &home, &project);
  assert!(
    trusted.contains("project  deploy\nDeploy this project\n"),
    "{trusted}"
  );
}

#[test]
fn an_expanded_prompt_is_the_only_thing_on_stdout_and_keeps_its_newlines() {
  let (_root, home, project) = fixture();
  let (stdout, stderr, ok) = run(&["prompt", "pr", "https://example.test/1"], &home, &project);
  assert!(ok, "{stderr}");
  assert_eq!(stdout, "Look at https://example.test/1.\n");
  assert!(
    !stdout.contains("[prompt]"),
    "the prompt is prose for a model: {stdout}"
  );
}

#[test]
fn a_template_argument_may_look_like_a_flag() {
  let (_root, home, project) = fixture();
  // Options are read before the name, so `--strict` after the name belongs to the
  // template rather than being rejected as an unknown flag of this command.
  fs::write(
    home.join(".pi/agent/prompts/lint.md"),
    "---\ndescription: Lint\n---\nlint $@\n",
  )
  .expect("write lint");
  let (stdout, stderr, ok) = run(&["prompt", "lint", "--strict"], &home, &project);
  assert!(ok, "{stderr}");
  assert_eq!(stdout, "lint --strict\n");
}

#[test]
fn a_name_that_is_not_there_is_an_error_that_names_what_to_run_instead() {
  let (_root, home, project) = fixture();
  let (stdout, stderr, ok) = run(&["prompt", "nope"], &home, &project);
  assert!(!ok);
  assert_eq!(stdout, "");
  assert!(
    stderr.contains("no prompt template named 'nope'") && stderr.contains("rupi prompts"),
    "{stderr}"
  );
}

#[test]
fn nothing_found_says_so_on_stderr_and_leaves_stdout_empty() {
  let root = tempfile::TempDir::new().expect("empty home");
  let (stdout, stderr, ok) = run(&["prompts"], root.path(), root.path());
  assert!(ok, "{stderr}");
  assert_eq!(stdout, "");
  assert!(stderr.contains("no prompt templates found"), "{stderr}");
}

#[test]
fn prompts_command_lists_package_prompts_with_package_attribution() {
  let pkg_home = Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("tests/compat/packages/home")
    .canonicalize()
    .expect("pkg home");
  let temp = tempfile::TempDir::new().expect("temp project");
  let (stdout, stderr, ok) = run(&["prompts"], &pkg_home, temp.path());
  assert!(ok, "{stderr}");
  assert!(
    stdout.contains("global  review (package: fixture-pkg-a)"),
    "{stdout}"
  );
  assert!(stdout.contains("Review prompt in package A"), "{stdout}");
}

#[test]
fn prompt_command_expands_package_template() {
  let pkg_home = Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("tests/compat/packages/home")
    .canonicalize()
    .expect("pkg home");
  let temp = tempfile::TempDir::new().expect("temp project");
  let (stdout, stderr, ok) = run(&["prompt", "review"], &pkg_home, temp.path());
  assert!(ok, "{stderr}");
  assert!(stdout.contains("Review this code carefully."), "{stdout}");
}

#[test]
fn prompt_command_supports_explicit_template_and_no_templates() {
  let (_root, home, project) = fixture();
  let custom = project.join("custom.md");
  fs::write(&custom, "Custom $1 template\n").expect("write custom template");

  let (stdout, stderr, ok) = run(
    &[
      "prompt",
      "--prompt-template",
      &custom.to_string_lossy(),
      "custom",
      "test",
    ],
    &home,
    &project,
  );
  assert!(ok, "{stderr}");
  assert_eq!(stdout, "Custom test template\n");

  let (stdout, stderr, ok) = run(&["prompts", "--no-prompt-templates"], &home, &project);
  assert!(ok, "{stderr}");
  assert!(stderr.contains("no prompt templates found"), "{stderr}");
  assert_eq!(stdout, "");
}
