//! Pi skills that already exist on disk, read from committed fixtures.
//!
//! The unit tests in `pi-rs-compat` build their trees in code. These fixtures are real
//! files in the repository, so a reviewer can read the input next to the expected
//! outcome, and so a later change to the rules shows up as a diff in a fixture rather
//! than only in a test.
//!
//! `tests/compat/skills/home` stands in for `$HOME`. Nothing here touches the real
//! home directory: `Discovery` takes the home path as an input.

use std::{
  fs,
  path::{Path, PathBuf},
};

use pi_rs_compat::skill::{self, Discovery, SkillWarning, Source, Trust};

fn home() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("tests/compat/skills/home")
    .canonicalize()
    .expect("fixture home")
}

fn scan(trust: Trust) -> skill::Scan {
  skill::discover(&Discovery {
    home: Some(home()),
    // A project directory that does not exist: this fixture is about the home family.
    cwd: home().join("not-a-project"),
    trust,
  })
}

#[test]
fn the_fixture_tree_loads_the_skills_pi_would_load() {
  let scan = scan(Trust::Untrusted);
  let names: Vec<&str> = scan
    .skills
    .iter()
    .map(|skill| skill.name.as_str())
    .collect();
  assert_eq!(names, ["dup", "loose", "pdf-tools", "team-notes"]);
  assert!(
    scan.named("no-description").is_none(),
    "a skill that cannot say what it does is never offered"
  );
}

#[test]
fn a_folded_description_becomes_the_one_line_the_model_sees() {
  let scan = scan(Trust::Untrusted);
  let skill = scan.named("pdf-tools").expect("fixture skill");
  assert_eq!(
    skill.description,
    "Extracts text and tables from PDF files. Fills forms, and merges documents."
  );
  assert_eq!(skill.license.as_deref(), Some("Apache-2.0"));
  assert_eq!(skill.source, Source::Global);
  assert!(skill.path.ends_with(".agents/skills/pdf-tools/SKILL.md"));
}

#[test]
fn the_optional_fields_survive_and_a_root_file_counts_in_the_dot_pi_family() {
  let scan = scan(Trust::Untrusted);
  let skill = scan.named("loose").expect("root file skill");
  assert!(skill.disable_model_invocation);
  assert_eq!(skill.allowed_tools, ["read", "grep"]);
}

#[test]
fn every_rejection_is_reported_once_with_its_path() {
  let warnings = scan(Trust::Untrusted).warnings;
  let relative: Vec<String> = warnings
    .iter()
    .map(|warning| match warning {
      SkillWarning::MissingDescription { path }
      | SkillWarning::MalformedFrontmatter { path }
      | SkillWarning::Duplicate { ignored: path, .. } => path
        .strip_prefix(home())
        .unwrap_or(path.as_path())
        .display()
        .to_string(),
      other => panic!("unexpected warning {other:?}"),
    })
    .collect();
  assert_eq!(
    relative,
    [
      ".agents/skills/broken/SKILL.md",
      ".agents/skills/dup/SKILL.md",
      ".agents/skills/no-description/SKILL.md",
    ]
  );
  // The duplicate names both files, so the operator can tell which one to edit.
  assert!(
    matches!(
      &warnings[1],
      SkillWarning::Duplicate { kept, .. } if kept.ends_with(".pi/agent/skills/dup.md")
    ),
    "{:?}",
    warnings[1]
  );
}

#[test]
fn documentation_in_a_skills_directory_is_neither_loaded_nor_reported() {
  let scan = scan(Trust::Untrusted);
  let paths: Vec<String> = scan
    .skills
    .iter()
    .map(|skill| skill.path.display().to_string())
    .collect();
  assert!(
    !paths.iter().any(|path| path.ends_with("README.md")),
    "{paths:?}"
  );
}

#[test]
fn project_locations_are_read_only_when_the_project_is_trusted() {
  // Its own project, not this repository: the fixture home has no project skills, so
  // the only difference between the two scans is the one skill written below.
  let temp = tempfile::TempDir::new().expect("temp project");
  let project = temp.path();
  let skill_dir = project.join(".agents/skills/repo-helper");
  fs::create_dir_all(&skill_dir).expect("fixture project skill");
  fs::write(
    skill_dir.join("SKILL.md"),
    "---\nname: repo-helper\ndescription: Helps in this repo.\n---\n",
  )
  .expect("write skill");

  let scan = |trust| {
    skill::discover(&Discovery {
      home: Some(home()),
      cwd: project.to_path_buf(),
      trust,
    })
  };
  let untrusted = scan(Trust::Untrusted);
  let trusted = scan(Trust::Trusted);

  assert!(
    untrusted.named("repo-helper").is_none(),
    "an untrusted checkout must not be able to hand the model instructions"
  );
  assert!(trusted.named("repo-helper").is_some(), "a trusted one must");
  assert_eq!(trusted.skills.len(), untrusted.skills.len() + 1);
  assert_eq!(
    untrusted.warnings, trusted.warnings,
    "the gate adds no warnings"
  );
}
