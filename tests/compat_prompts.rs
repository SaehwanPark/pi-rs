//! Pi prompt templates that already exist on disk, read from committed fixtures.
//!
//! The unit tests in `pi-rs-compat` build their trees in code. These fixtures are real
//! files in the repository, so a reviewer can read the input beside the expected outcome,
//! and so a later change to the rules shows up as a diff in a fixture rather than only in
//! a test.
//!
//! `tests/compat/prompts/home` stands in for `$HOME`. Nothing here touches the real home
//! directory: `Discovery` takes the home path as an input.

use std::path::{Path, PathBuf};

use pi_rs_compat::{
  prompt::{self, Warning},
  scan::{Discovery, Source, Trust},
};

fn home() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("tests/compat/prompts/home")
    .canonicalize()
    .expect("fixture home")
}

fn scan() -> prompt::Scan {
  prompt::discover(&Discovery {
    home: Some(home()),
    // A project directory that does not exist: this fixture is about the home family.
    cwd: home().join("not-a-project"),
    trust: Trust::Untrusted,
  })
}

fn names(scan: &prompt::Scan) -> Vec<String> {
  scan
    .templates
    .iter()
    .map(|template| template.name.clone())
    .collect()
}

/// What was declined, as `<kind>:<filename>`, in the order the scan reported it.
fn declined(scan: &prompt::Scan) -> Vec<String> {
  scan
    .warnings
    .iter()
    .map(|warning| {
      let (kind, path) = match warning {
        Warning::MalformedFrontmatter { path } => ("malformed", path),
        Warning::Empty { path } => ("empty", path),
        Warning::Unreadable { path, .. } => ("unreadable", path),
        Warning::Duplicate { name, .. } => return format!("duplicate:{name}"),
      };
      let file = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
      format!("{kind}:{file}")
    })
    .collect()
}

#[test]
fn the_fixture_tree_loads_the_templates_pi_would_load() {
  let scan = scan();
  assert_eq!(names(&scan), ["plain", "pr", "review"]);
  assert_eq!(
    scan
      .templates
      .iter()
      .map(|template| template.source)
      .collect::<Vec<_>>()
      .as_slice(),
    &[Source::Global, Source::Global, Source::Global]
  );
}

#[test]
fn a_declared_description_is_the_one_the_author_wrote() {
  let scan = scan();
  let review = scan.named("review").expect("fixture template");
  assert_eq!(review.description, "Review staged git changes");
  assert!(!review.description_from_body);
  assert_eq!(review.argument_hint, None);
  assert!(review.body.starts_with("Review the staged changes"));
  assert!(review.path.ends_with(".pi/agent/prompts/review.md"));
}

#[test]
fn a_template_without_frontmatter_is_still_a_template() {
  // Pi takes the name from the filename and, with no `description`, the first non-empty
  // line. The body is the whole file, frontmatter-free because there is none.
  let scan = scan();
  let plain = scan.named("plain").expect("fixture template");
  assert_eq!(
    plain.description,
    "Summarize what changed today in the current branch."
  );
  assert!(plain.description_from_body);
  assert!(
    plain.body.starts_with("Summarize"),
    "the blank lines that were there to be folded away are not part of the prompt: {}",
    plain.body
  );
}

#[test]
fn an_argument_hint_is_kept_for_the_surface_that_displays_it() {
  let scan = scan();
  let pr = scan.named("pr").expect("fixture template");
  assert_eq!(pr.argument_hint.as_deref(), Some("<PR-URL>"));
  assert_eq!(
    pr.expand(&["https://example.test/pull/12"]),
    "Analyze the pull request at https://example.test/pull/12."
  );
}

#[test]
fn a_file_that_cannot_become_a_prompt_is_named_once_each() {
  let scan = scan();
  assert_eq!(declined(&scan), ["empty:empty.md", "malformed:unclosed.md"]);
}

#[test]
fn what_pi_skips_without_comment_is_skipped_without_comment() {
  // `*.md`, non-recursive: Pi documents both rules and says nothing when they skip a file,
  // so a warning on every run would teach the user to ignore warnings. If these files
  // ever show up in either list -- or vanish from the tree without the listing changing --
  // the rule and this test have parted company.
  let scan = scan();
  assert!(
    !names(&scan)
      .iter()
      .any(|name| name == "inner" || name == "notes"),
    "{:?}",
    names(&scan)
  );
  assert_eq!(declined(&scan).len(), 2, "{:?}", scan.warnings);
}

#[test]
fn expansion_reaches_the_whole_document_not_just_the_first_line() {
  let scan = scan();
  let review = scan.named("review").expect("fixture template");
  let expanded = review.expand(&["auth", "and", "tests"]);
  assert!(
    expanded.contains("Bugs and logic errors in auth"),
    "{expanded}"
  );
  assert!(
    expanded.contains("Error handling gaps: auth and tests"),
    "{expanded}"
  );
}
