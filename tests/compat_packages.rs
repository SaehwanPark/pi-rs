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

// ---------------------------------------------------------------------------
// Package discovery driven by committed fixture directories
// ---------------------------------------------------------------------------

use pi_rs_compat::scan::{Discovery, Source, Trust};

fn fixture_home() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("tests/compat/packages/home")
    .canonicalize()
    .expect("fixture home")
}

fn fixture_project() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("tests/compat/packages/project")
    .canonicalize()
    .expect("fixture project")
}

#[test]
fn fixture_discovery_untrusted_finds_only_home_packages() {
  let discovery = Discovery {
    home: Some(fixture_home()),
    cwd: fixture_project(),
    trust: Trust::Untrusted,
  };
  let scan = package::discover(&discovery);

  let names: Vec<&str> = scan.packages.iter().map(|p| p.name.as_str()).collect();
  assert_eq!(names, ["fixture-pkg-a", "fixture-pkg-ext"]);
  assert!(scan.named("project-pkg").is_none());

  // Warned about missing-manifest subdirectory
  assert!(
    scan
      .warnings
      .iter()
      .any(|w| matches!(w, Warning::MissingManifest { .. }))
  );
}

#[test]
fn fixture_discovery_trusted_finds_home_and_project_packages() {
  let discovery = Discovery {
    home: Some(fixture_home()),
    cwd: fixture_project(),
    trust: Trust::Trusted,
  };
  let scan = package::discover(&discovery);

  let names: Vec<&str> = scan.packages.iter().map(|p| p.name.as_str()).collect();
  assert_eq!(names, ["fixture-pkg-a", "fixture-pkg-ext", "project-pkg"]);

  let proj = scan.named("project-pkg").expect("project pkg found");
  assert_eq!(proj.source, Source::Project);
  assert_eq!(proj.version.as_deref(), Some("2.1.0"));
}

#[test]
fn fixture_package_exposes_contained_skills_and_prompts() {
  let discovery = Discovery {
    home: Some(fixture_home()),
    cwd: fixture_project(),
    trust: Trust::Untrusted,
  };
  let scan = package::discover(&discovery);
  let pkg = scan.named("fixture-pkg-a").expect("found pkg a");

  let skills = pkg.skill_locations();
  assert_eq!(skills.len(), 1);
  assert!(skills[0].ends_with("skills"));

  let prompts = pkg.prompt_locations();
  assert_eq!(prompts.len(), 1);
  assert!(prompts[0].ends_with("prompts/review.md"));

  let surfaces = pkg.surfaces();
  assert_eq!(
    surfaces,
    vec![
      ("skills", package::SurfaceStatus::Supported),
      ("prompts", package::SurfaceStatus::Supported),
      ("extensions", package::SurfaceStatus::NotPresent),
    ]
  );
}

#[test]
fn fixture_package_reports_extensions_as_unsupported_surface() {
  let discovery = Discovery {
    home: Some(fixture_home()),
    cwd: fixture_project(),
    trust: Trust::Untrusted,
  };
  let scan = package::discover(&discovery);
  let pkg = scan.named("fixture-pkg-ext").expect("found pkg ext");

  assert!(pkg.has_extensions());
  let surfaces = pkg.surfaces();
  assert_eq!(
    surfaces,
    vec![
      ("skills", package::SurfaceStatus::NotPresent),
      ("prompts", package::SurfaceStatus::NotPresent),
      ("extensions", package::SurfaceStatus::Unsupported),
    ]
  );
}
