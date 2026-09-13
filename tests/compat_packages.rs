//! Pi-compatible package manifest parsing, driven from committed fixture files.
//!
//! The unit tests in `pi-rs-compat` build their inputs in code. These fixtures are real
//! files in the repository, so a reviewer can read the input next to the expected
//! outcome, and so a later change to the rules shows up as a diff in a fixture rather
//! than only in a test.
//!
//! `tests/compat/packages/` holds one subdirectory per fixture package, each containing
//! a `package.json` that exercises a different compatibility scenario.

use std::path::{Path, PathBuf};

use pi_rs_compat::package::{self, Warning};

fn fixture(name: &str) -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("tests/compat/packages")
    .join(name)
    .join("package.json")
}

// ---------------------------------------------------------------------------
// minimal: name + version, no Pi surfaces
// ---------------------------------------------------------------------------

#[test]
fn minimal_manifest_extracts_name_and_version_with_no_warnings() {
  let p = package::read(&fixture("minimal"));
  assert!(
    p.warnings.is_empty(),
    "minimal manifest should produce no warnings: {:?}",
    p.warnings
  );
  assert_eq!(p.manifest.name.as_deref(), Some("minimal-pkg"));
  assert_eq!(p.manifest.version.as_deref(), Some("1.0.0"));
  assert!(p.manifest.skill_paths.is_empty());
  assert!(p.manifest.prompt_paths.is_empty());
}

// ---------------------------------------------------------------------------
// full: pi.skills + pi.prompts + npm metadata — no warnings
// ---------------------------------------------------------------------------

#[test]
fn full_manifest_extracts_skills_and_prompts_with_no_warnings() {
  let p = package::read(&fixture("full"));
  assert!(
    p.warnings.is_empty(),
    "full manifest should produce no warnings, got: {:?}",
    p.warnings
  );
  assert_eq!(p.manifest.name.as_deref(), Some("full-pkg"));
  assert_eq!(p.manifest.version.as_deref(), Some("2.0.0"));
  assert_eq!(
    p.manifest.description.as_deref(),
    Some("A full-featured Pi package with skills and prompts.")
  );
  assert_eq!(p.manifest.skill_paths, ["skills/", "skills/pdf-tools"]);
  assert_eq!(
    p.manifest.prompt_paths,
    ["prompts/review.md", "prompts/plan.md"]
  );
}

// ---------------------------------------------------------------------------
// extensions-only: skills extracted, extensions produce a warning
// ---------------------------------------------------------------------------

#[test]
fn extensions_manifest_extracts_skills_and_warns_on_extensions() {
  let p = package::read(&fixture("extensions-only"));
  assert_eq!(p.manifest.name.as_deref(), Some("ext-pkg"));
  assert_eq!(p.manifest.skill_paths, ["skills/"]);
  assert_eq!(p.warnings.len(), 1, "expected exactly one warning");
  assert!(
    matches!(
      &p.warnings[0],
      Warning::UnsupportedSurface { surface, count }
        if surface == "extensions" && *count == 2
    ),
    "unexpected warning: {:?}",
    p.warnings[0]
  );
}

// ---------------------------------------------------------------------------
// malformed: invalid JSON produces a single Malformed warning
// ---------------------------------------------------------------------------

#[test]
fn malformed_manifest_produces_a_malformed_warning() {
  let p = package::read(&fixture("malformed"));
  assert_eq!(
    p.warnings.len(),
    1,
    "expected exactly one warning, got: {:?}",
    p.warnings
  );
  assert!(
    matches!(p.warnings[0], Warning::Malformed { .. }),
    "expected Malformed warning, got: {:?}",
    p.warnings[0]
  );
  // Manifest data before the error is discarded; name was before the error position
  // so it may or may not be set depending on parse order. Either way is acceptable;
  // the guarantee is that the error is reported, not the partial state.
}

// ---------------------------------------------------------------------------
// Missing file produces an Unreadable warning
// ---------------------------------------------------------------------------

#[test]
fn missing_file_produces_an_unreadable_warning() {
  let missing =
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/compat/packages/does-not-exist/package.json");
  let p = package::read(&missing);
  assert_eq!(
    p.warnings.len(),
    1,
    "expected exactly one warning, got: {:?}",
    p.warnings
  );
  assert!(
    matches!(p.warnings[0], Warning::Unreadable { .. }),
    "expected Unreadable warning, got: {:?}",
    p.warnings[0]
  );
}
