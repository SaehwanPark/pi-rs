//! Finding Pi-style skills, and saying honestly what was not a skill.
//!
//! A skill is a directory with a `SKILL.md`, or in some locations a bare `.md` file
//! with skill frontmatter. Pi scans several places, warns about what it does not
//! understand, and loads the rest. This module reproduces that behavior for the two
//! families Pi documents as file locations -- per-user and per-project -- and reports
//! every decision it made about a candidate file.
//!
//! # Locations, and the order they are read in
//!
//! ```text
//! $HOME/.pi/agent/skills      root *.md files count as individual skills
//! $HOME/.agents/skills        root *.md files ignored; nested ones in grouping dirs count
//! <ancestor>/.pi/skills       as above, project family
//! <ancestor>/.agents/skills   as above, project family
//! ```
//!
//! Ancestors run from `cwd` upward through the git root (or the filesystem root when
//! `cwd` is not in a repository), nearest first. Scan order matters: a name declared
//! twice resolves to whoever was found first, and Pi documents first-found-wins, so
//! the order above and the sorting described below are what make that resolution the
//! same on every machine rather than whatever `readdir` returned.
//!
//! Package `skills/` directories, `package.json` entries, the `skills` array in
//! settings, and `--skill` paths are other sources in Pi and are not implemented yet.
//!
//! # Trust
//!
//! Project locations are read only when the caller says the project is trusted. Pi
//! loads project skills only after the project is trusted, and for good reason: a
//! skill is instructions for the model, so reading one from an untrusted checkout is
//! handing that checkout the keyboard. [`Trust`] is an input rather than a filesystem
//! lookup because `pi-rs` has no trust decision to consult yet -- whoever grows one
//! passes its answer here.
//!
//! # Bounds
//!
//! Directory walks stop at [`MAX_DEPTH`] with a warning. Symbolic links are followed,
//! because linking in a skill kept elsewhere is a normal way to share one and the trust
//! decision was already made about the directory holding the link; the depth bound is
//! what stops a link that points back at an ancestor, and it reports rather than
//! stopping quietly. A skill's own subdirectories are its resources and are not scanned
//! for further skills: `references/foo/SKILL.md` inside a skill is a reference, not a
//! second skill wearing the first one's clothes.

use std::{
  collections::BTreeSet,
  fs,
  path::{Path, PathBuf},
};

use crate::frontmatter::{self, Extract};

/// Longest `name` the Agent Skills standard allows.
pub const MAX_NAME_CHARS: usize = 64;
/// Longest `description` the Agent Skills standard allows.
pub const MAX_DESCRIPTION_CHARS: usize = 1024;
/// How deep a skill location is walked before the scan gives up and says so.
pub const MAX_DEPTH: usize = 8;

/// Whether the project's own files may be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
  /// The operator has accepted this project.
  Trusted,
  /// Nobody has. Project locations are skipped entirely.
  Untrusted,
}

/// Which kind of location a skill came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
  /// Under `$HOME`: the user's own configuration, not something a repository supplied.
  Global,
  /// Under the working directory or an ancestor of it, including an inner repository
  /// inside a larger one.
  Project,
}

impl SkillSource {
  /// Stable machine label.
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Global => "global",
      Self::Project => "project",
    }
  }
}

/// A skill that was loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
  pub name: String,
  pub description: String,
  /// The file it was read from: `SKILL.md`, or a `.md` file in a location that accepts
  /// them. Relative references inside the skill resolve against its parent directory.
  pub path: PathBuf,
  pub source: SkillSource,
  pub license: Option<String>,
  pub compatibility: Option<String>,
  /// `allowed-tools`, split on spaces. Pi calls this experimental; nothing here acts
  /// on it, it records it so a later authorization step has something to authorize.
  pub allowed_tools: Vec<String>,
  /// `disable-model-invocation`. Such a skill never enters the model-facing prompt and
  /// is reachable only by explicit request.
  pub disable_model_invocation: bool,
}

/// Something worth telling the operator about a candidate that was not loaded, or was
/// loaded with a defect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillWarning {
  /// No `name`, so there is nothing to call the skill by. Not loaded.
  MissingName { path: PathBuf },
  /// Pi does not load a declared skill that cannot say what it does, and neither does
  /// this: the description is the only thing the model sees before deciding to read the
  /// file, so a skill without one is a skill that is never usable.
  MissingDescription { path: PathBuf },
  /// A `---` block that never closed. Not loaded: the body would have been read as
  /// frontmatter, and a name could then be found anywhere in the file.
  MalformedFrontmatter { path: PathBuf },
  /// The name breaks the standard's spelling rules. Loaded anyway, as Pi does.
  InvalidName {
    path: PathBuf,
    name: String,
    reason: &'static str,
  },
  /// Longer than the standard allows. Loaded anyway.
  NameTooLong { path: PathBuf, name: String },
  /// Longer than the standard allows. Loaded anyway.
  DescriptionTooLong { path: PathBuf, name: String },
  /// Two locations declared one name. The first found was kept, this one was dropped.
  Duplicate {
    name: String,
    kept: PathBuf,
    ignored: PathBuf,
  },
  /// The file exists and could not be read: permissions, or bytes that are not text.
  Unreadable { path: PathBuf, reason: String },
  /// The walk stopped at the depth limit, so anything below it went unseen.
  TooDeep { path: PathBuf },
}

/// What a scan found, and everything it declined.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scan {
  pub skills: Vec<Skill>,
  pub warnings: Vec<SkillWarning>,
}

impl Scan {
  /// The kept skill with this name, if one was loaded.
  pub fn named(&self, name: &str) -> Option<&Skill> {
    self.skills.iter().find(|skill| skill.name == name)
  }
}

/// Where to look, and under what assumptions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
  /// `$HOME` if known. Without it, only project locations are read.
  pub home: Option<PathBuf>,
  /// The working directory the project is anchored at.
  pub cwd: PathBuf,
  pub trust: Trust,
}

impl Discovery {
  /// Global locations plus the project at `cwd`, untrusted.
  pub fn new(cwd: impl Into<PathBuf>) -> Self {
    Self {
      home: home_dir(),
      cwd: cwd.into(),
      trust: Trust::Untrusted,
    }
  }

  /// Read the project's own locations too.
  pub fn trusted(mut self) -> Self {
    self.trust = Trust::Trusted;
    self
  }
}

/// `$HOME`, falling back to the Windows spelling.
pub fn home_dir() -> Option<PathBuf> {
  ["HOME", "USERPROFILE"]
    .iter()
    .find_map(std::env::var_os)
    .map(PathBuf::from)
    .filter(|home| home.is_absolute())
}

/// Scan the documented locations.
pub fn discover(discovery: &Discovery) -> Scan {
  let mut scanner = Scanner::default();
  if let Some(home) = &discovery.home {
    scanner.location(
      &home.join(".pi/agent/skills"),
      SkillSource::Global,
      Family::DotPi,
    );
    scanner.location(
      &home.join(".agents/skills"),
      SkillSource::Global,
      Family::Agents,
    );
  }
  if discovery.trust == Trust::Trusted {
    for project in project_dirs(&discovery.cwd) {
      scanner.location(
        &project.join(".pi/skills"),
        SkillSource::Project,
        Family::DotPi,
      );
      scanner.location(
        &project.join(".agents/skills"),
        SkillSource::Project,
        Family::Agents,
      );
    }
  }
  scanner.finish()
}

/// The project roots whose skill locations are read: `cwd` upward through the git root,
/// or to the filesystem root when there is no repository.
fn project_dirs(cwd: &Path) -> Vec<PathBuf> {
  let ceiling = git_root(cwd);
  let mut dirs = Vec::new();
  let mut current = Some(cwd.to_path_buf());
  while let Some(dir) = current {
    let at_ceiling = ceiling.as_deref() == Some(dir.as_path());
    current = dir.parent().map(Path::to_path_buf);
    dirs.push(dir);
    if at_ceiling {
      break;
    }
  }
  dirs
}

fn git_root(cwd: &Path) -> Option<PathBuf> {
  cwd
    .ancestors()
    .find(|dir| dir.join(".git").exists())
    .map(Path::to_path_buf)
}

/// How a file is treated inside one location family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
  /// `.pi/skills`, `~/.pi/agent/skills`: root `.md` files are individual skills.
  DotPi,
  /// `.agents/skills`, `~/.agents/skills`: shared with other tools, so a loose `.md` at
  /// the root is a note, and only files inside a grouping directory are candidates.
  Agents,
}

impl Family {
  /// Whether a `.md` file at this depth is a candidate.
  fn accepts(&self, at_root: bool) -> bool {
    match self {
      Self::DotPi => at_root,
      Self::Agents => !at_root,
    }
  }
}

#[derive(Default)]
struct Scanner {
  skills: Vec<Skill>,
  warnings: Vec<SkillWarning>,
  names: BTreeSet<String>,
}

impl Scanner {
  fn location(&mut self, root: &Path, source: SkillSource, family: Family) {
    if root.is_dir() {
      self.walk(root, true, 0, source, family);
    }
  }

  fn walk(&mut self, dir: &Path, at_root: bool, depth: usize, source: SkillSource, family: Family) {
    if depth > MAX_DEPTH {
      self.warnings.push(SkillWarning::TooDeep {
        path: dir.to_path_buf(),
      });
      return;
    }
    let entries = match fs::read_dir(dir) {
      Ok(entries) => entries,
      Err(error) => {
        self.warnings.push(SkillWarning::Unreadable {
          path: dir.to_path_buf(),
          reason: error.to_string(),
        });
        return;
      }
    };
    // Directory order is whatever the filesystem returns. Sorting makes first-found
    // collision resolution -- and therefore every test -- reproducible.
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
      let path = entry.path();
      // `metadata`, not the directory entry's file type: this one follows symbolic
      // links, so a skill kept elsewhere and linked in is a skill. Refusing links would
      // break the normal way to share a skill between checkouts, and the trust decision
      // was already made about the directory holding the link. A link that points back
      // at an ancestor is a cycle, and the depth bound is what stops the walk -- and
      // says so -- rather than a visited set this scan does not need for any other
      // reason. Still only regular files and directories are read: `is_file` is false
      // for a fifo, and opening one would block this scan forever.
      let kind = path.metadata().ok();
      if is_dir(&kind) {
        let skill_md = path.join("SKILL.md");
        if skill_md.is_file() {
          // A skill's own subdirectories hold its scripts and references.
          self.collect(&skill_md, source);
        } else {
          self.walk(&path, false, depth + 1, source, family);
        }
      } else if is_file(&kind) {
        let name = entry.file_name().to_string_lossy().into_owned();
        // `SKILL.md` is the canonical skill file and counts wherever it appears; the
        // family rule decides what any other `.md` file is.
        if name == "SKILL.md" || (name.ends_with(".md") && family.accepts(at_root)) {
          self.collect(&path, source);
        }
      }
    }
  }

  fn collect(&mut self, path: &Path, source: SkillSource) {
    let text = match fs::read_to_string(path) {
      Ok(text) => text,
      Err(error) => {
        self.warnings.push(SkillWarning::Unreadable {
          path: path.to_path_buf(),
          reason: error.to_string(),
        });
        return;
      }
    };
    let frontmatter = match frontmatter::extract(&text) {
      Extract::Found(frontmatter) => frontmatter,
      // Most `.md` files in a skills directory are documentation, and Pi ignores them.
      Extract::Absent => return,
      Extract::Malformed(_) => {
        self.warnings.push(SkillWarning::MalformedFrontmatter {
          path: path.to_path_buf(),
        });
        return;
      }
    };
    let name = frontmatter.get("name").map(str::trim).unwrap_or("");
    if name.is_empty() {
      self.warnings.push(SkillWarning::MissingName {
        path: path.to_path_buf(),
      });
      return;
    }
    // Pi warns about a name that breaks the standard and loads the skill anyway. A name
    // that is *absent* is different: naming it for the author would put a name in front
    // of the user that nobody wrote, and `/skill:<name>` is spelled by hand.
    self.check_name(path, name);
    let description = frontmatter.get("description").map(str::trim).unwrap_or("");
    if description.is_empty() {
      self.warnings.push(SkillWarning::MissingDescription {
        path: path.to_path_buf(),
      });
      return;
    }
    if description.chars().count() > MAX_DESCRIPTION_CHARS {
      self.warnings.push(SkillWarning::DescriptionTooLong {
        path: path.to_path_buf(),
        name: name.to_string(),
      });
    }
    if !self.names.insert(name.to_string()) {
      let kept = self
        .skills
        .iter()
        .find(|skill| skill.name == name)
        .map(|skill| skill.path.clone())
        .unwrap_or_default();
      self.warnings.push(SkillWarning::Duplicate {
        name: name.to_string(),
        kept,
        ignored: path.to_path_buf(),
      });
      return;
    }
    self.skills.push(Skill {
      name: name.to_string(),
      description: description.to_string(),
      path: path.to_path_buf(),
      source,
      license: nonempty(frontmatter.get("license")),
      compatibility: nonempty(frontmatter.get("compatibility")),
      allowed_tools: frontmatter
        .get("allowed-tools")
        .map(|value| value.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default(),
      disable_model_invocation: frontmatter.is_true("disable-model-invocation"),
    });
  }

  /// Warnings about a name, without rejecting it.
  fn check_name(&mut self, path: &Path, name: &str) {
    if name.chars().count() > MAX_NAME_CHARS {
      self.warnings.push(SkillWarning::NameTooLong {
        path: path.to_path_buf(),
        name: name.to_string(),
      });
      return;
    }
    let reason = if !name
      .chars()
      .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
    {
      Some("lowercase letters, digits and hyphens only")
    } else if name.starts_with('-') || name.ends_with('-') {
      Some("must not start or end with a hyphen")
    } else if name.contains("--") {
      Some("must not contain consecutive hyphens")
    } else {
      None
    };
    if let Some(reason) = reason {
      self.warnings.push(SkillWarning::InvalidName {
        path: path.to_path_buf(),
        name: name.to_string(),
        reason,
      });
    }
  }

  fn finish(self) -> Scan {
    Scan {
      skills: self.skills,
      warnings: self.warnings,
    }
  }
}

fn is_dir(kind: &Option<fs::Metadata>) -> bool {
  kind.as_ref().is_some_and(fs::Metadata::is_dir)
}

fn is_file(kind: &Option<fs::Metadata>) -> bool {
  kind.as_ref().is_some_and(fs::Metadata::is_file)
}

fn nonempty(value: Option<&str>) -> Option<String> {
  value
    .map(str::trim)
    .filter(|value| !value.is_empty())
    .map(str::to_string)
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A skill file written where the caller says, named after its directory.
  fn skill_file(dir: &Path, file: &str, name: &str, description: &str) -> PathBuf {
    let path = dir.join(file);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
      &path,
      format!("---\nname: {name}\ndescription: {description}\n---\n\nBody\n"),
    )
    .unwrap();
    path
  }

  /// A scenario with its own fake `$HOME`, its own project, and no shared state.
  struct Fixture {
    /// The scenario's root. Held so the directories outlive the scan.
    root: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
  }

  impl Fixture {
    fn new() -> Self {
      let temp = tempfile::TempDir::new().unwrap();
      let home = temp.path().join("home");
      let project = temp.path().join("project");
      fs::create_dir_all(&home).unwrap();
      fs::create_dir_all(&project).unwrap();
      Self {
        root: temp,
        home,
        project,
      }
    }

    fn discovery(&self) -> Discovery {
      Discovery {
        home: Some(self.home.clone()),
        cwd: self.project.clone(),
        trust: Trust::Untrusted,
      }
    }
  }

  #[test]
  fn a_directory_with_a_skill_md_is_a_skill() {
    let fixture = Fixture::new();
    skill_file(
      &fixture.home.join(".agents/skills/pdf-tools"),
      "SKILL.md",
      "pdf-tools",
      "Extracts text from PDFs.",
    );
    let scan = discover(&fixture.discovery());
    let skill = scan.named("pdf-tools").expect("skill loaded");
    assert_eq!(skill.source, SkillSource::Global);
    assert!(skill.path.ends_with(".agents/skills/pdf-tools/SKILL.md"));
    assert!(scan.warnings.is_empty(), "{:?}", scan.warnings);
  }

  #[test]
  fn the_dot_pi_family_takes_root_markdown_files_the_agents_family_does_not() {
    let fixture = Fixture::new();
    skill_file(
      &fixture.home.join(".pi/agent/skills"),
      "loose.md",
      "dot-pi-loose",
      "A root file in a .pi location.",
    );
    skill_file(
      &fixture.home.join(".agents/skills"),
      "loose.md",
      "agents-loose",
      "A root file in a shared location.",
    );
    let scan = discover(&fixture.discovery());
    assert!(
      scan.named("dot-pi-loose").is_some(),
      "root files count in .pi"
    );
    assert!(
      scan.named("agents-loose").is_none(),
      "a loose note at the root of a shared directory is not a skill: {:?}",
      scan.skills
    );
    // It is skipped without comment, exactly as Pi skips it.
    assert!(scan.warnings.is_empty(), "{:?}", scan.warnings);
  }

  #[test]
  fn a_grouping_directory_holds_skills_for_the_shared_family() {
    let fixture = Fixture::new();
    skill_file(
      &fixture.home.join(".agents/skills/team-tools"),
      "notes.md",
      "team-notes",
      "Notes-shaped skill inside a grouping directory.",
    );
    let scan = discover(&fixture.discovery());
    assert!(scan.named("team-notes").is_some(), "{:?}", scan.skills);
  }

  #[test]
  fn documentation_inside_a_skill_is_not_a_second_skill() {
    let fixture = Fixture::new();
    let skill = fixture.home.join(".agents/skills/pdf-tools");
    skill_file(&skill, "SKILL.md", "pdf-tools", "Extracts text from PDFs.");
    skill_file(
      &skill.join("references"),
      "api.md",
      "api-reference",
      "Looks like a skill, is a reference.",
    );
    let scan = discover(&fixture.discovery());
    assert_eq!(scan.skills.len(), 1, "{:?}", scan.skills);
    assert!(scan.named("api-reference").is_none());
  }

  #[test]
  fn prose_without_frontmatter_is_ignored_in_silence() {
    let fixture = Fixture::new();
    let readme = fixture.home.join(".agents/skills/README.md");
    fs::create_dir_all(readme.parent().unwrap()).unwrap();
    fs::write(&readme, "# Skills here\n\nNothing to load.\n").unwrap();
    let scan = discover(&fixture.discovery());
    assert!(scan.skills.is_empty());
    assert!(scan.warnings.is_empty(), "{:?}", scan.warnings);
  }

  #[test]
  fn a_project_skill_is_read_only_when_the_project_is_trusted() {
    let fixture = Fixture::new();
    skill_file(
      &fixture.project.join(".agents/skills/repo-helper"),
      "SKILL.md",
      "repo-helper",
      "Helps in this repo.",
    );
    let untrusted = discover(&fixture.discovery());
    assert!(untrusted.skills.is_empty(), "{:?}", untrusted.skills);
    let discovery = fixture.discovery().trusted();
    let trusted = discover(&discovery);
    let skill = trusted.named("repo-helper").expect("trusted project skill");
    assert_eq!(skill.source, SkillSource::Project);
  }

  #[test]
  fn ancestors_are_read_up_to_the_git_root_and_no_further() {
    let fixture = Fixture::new();
    let outer = fixture.project.join("outer");
    let inner = outer.join("inner");
    fs::create_dir_all(inner.join(".git")).unwrap();
    skill_file(
      &inner.join(".agents/skills/inner-helper"),
      "SKILL.md",
      "inner-helper",
      "Inside the repository.",
    );
    skill_file(
      &outer.join(".agents/skills/outer-helper"),
      "SKILL.md",
      "outer-helper",
      "Outside the repository.",
    );
    let mut discovery = fixture.discovery();
    discovery.cwd = inner;
    discovery.trust = Trust::Trusted;
    let scan = discover(&discovery);
    assert!(scan.named("inner-helper").is_some(), "{:?}", scan.skills);
    assert!(
      scan.named("outer-helper").is_none(),
      "the repository boundary ends the walk: {:?}",
      scan.skills
    );
  }

  #[test]
  fn the_first_skill_to_claim_a_name_keeps_it() {
    let fixture = Fixture::new();
    let kept = fixture.home.join(".pi/agent/skills/clash.md");
    fs::create_dir_all(kept.parent().unwrap()).unwrap();
    fs::write(&kept, "---\nname: clash\ndescription: Global first.\n---\n").unwrap();
    skill_file(
      &fixture.home.join(".agents/skills/clash"),
      "SKILL.md",
      "clash",
      "Same name, later location.",
    );
    let scan = discover(&fixture.discovery());
    assert_eq!(scan.skills.len(), 1);
    assert_eq!(
      scan.warnings,
      vec![SkillWarning::Duplicate {
        name: "clash".into(),
        kept,
        ignored: fixture.home.join(".agents/skills/clash/SKILL.md"),
      }]
    );
  }

  #[test]
  fn the_depth_limit_is_a_warning_not_silence() {
    let fixture = Fixture::new();
    let mut deep = fixture.home.join(".agents/skills");
    for level in 0..=MAX_DEPTH {
      deep = deep.join(format!("g{level}"));
    }
    skill_file(&deep, "buried.md", "buried", "Below the depth limit.");
    let scan = discover(&fixture.discovery());
    assert!(scan.skills.is_empty(), "{:?}", scan.skills);
    assert!(
      matches!(&scan.warnings[..], [SkillWarning::TooDeep { .. }]),
      "{:?}",
      scan.warnings
    );
  }

  #[test]
  fn a_skill_without_a_description_is_not_loaded_and_says_why() {
    let fixture = Fixture::new();
    let path = fixture.home.join(".agents/skills/quiet/SKILL.md");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "---\nname: quiet\n---\n\nNo description.\n").unwrap();
    let scan = discover(&fixture.discovery());
    assert!(scan.skills.is_empty());
    assert_eq!(
      scan.warnings,
      vec![SkillWarning::MissingDescription { path }]
    );
  }

  #[test]
  fn a_name_that_breaks_the_standard_loads_with_a_warning() {
    let fixture = Fixture::new();
    let path = fixture.home.join(".agents/skills/bad/SKILL.md");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
      &path,
      "---\nname: PDF-Processing\ndescription: Badly named.\n---\n",
    )
    .unwrap();
    let scan = discover(&fixture.discovery());
    assert!(
      scan.named("PDF-Processing").is_some(),
      "warn, do not reject"
    );
    assert!(
      matches!(&scan.warnings[..],
        [SkillWarning::InvalidName { reason, .. }]
          if reason == &"lowercase letters, digits and hyphens only"),
      "{:?}",
      scan.warnings
    );
  }

  #[test]
  fn an_open_frontmatter_block_is_reported() {
    let fixture = Fixture::new();
    let path = fixture.home.join(".agents/skills/broken/SKILL.md");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "---\nname: broken\ndescription: never closes\n").unwrap();
    let scan = discover(&fixture.discovery());
    assert_eq!(
      scan.warnings,
      vec![SkillWarning::MalformedFrontmatter { path }]
    );
  }

  #[test]
  fn an_oversized_description_warns_and_loads() {
    let fixture = Fixture::new();
    let long = "x".repeat(MAX_DESCRIPTION_CHARS + 1);
    skill_file(
      &fixture.home.join(".agents/skills/wordy"),
      "SKILL.md",
      "wordy",
      &long,
    );
    let scan = discover(&fixture.discovery());
    assert!(scan.named("wordy").is_some());
    assert!(matches!(
      &scan.warnings[..],
      [SkillWarning::DescriptionTooLong { .. }]
    ));
  }

  #[test]
  fn the_optional_fields_are_recorded_rather_than_dropped() {
    let fixture = Fixture::new();
    let path = fixture.home.join(".agents/skills/private/SKILL.md");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
      &path,
      "---\nname: private\ndescription: Only on request.\ndisable-model-invocation: true\nlicense: MIT\nallowed-tools: read grep\n---\n",
    )
    .unwrap();
    let scan = discover(&fixture.discovery());
    let skill = scan.named("private").expect("loaded");
    assert!(skill.disable_model_invocation);
    assert_eq!(skill.license.as_deref(), Some("MIT"));
    assert_eq!(skill.allowed_tools, vec!["read", "grep"]);
  }

  #[test]
  fn a_missing_home_is_not_a_failure() {
    let fixture = Fixture::new();
    let mut discovery = fixture.discovery();
    discovery.home = None;
    let scan = discover(&discovery);
    assert!(scan.skills.is_empty());
    assert!(scan.warnings.is_empty(), "{:?}", scan.warnings);
  }

  #[cfg(unix)]
  #[test]
  fn a_linked_skill_is_a_skill() {
    // The normal way to share one skill between checkouts: keep it somewhere else and
    // link it into the location. Refusing links would silently lose it.
    let fixture = Fixture::new();
    let elsewhere = fixture.root.path().join("shared/pdf-tools");
    skill_file(
      &elsewhere,
      "SKILL.md",
      "pdf-tools",
      "Lives elsewhere, linked in.",
    );
    let linked = fixture.home.join(".agents/skills/pdf");
    fs::create_dir_all(linked.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &linked).expect("symlink");
    let scan = discover(&fixture.discovery());
    assert!(scan.named("pdf-tools").is_some(), "{:?}", scan.skills);
  }

  #[cfg(unix)]
  #[test]
  fn a_link_that_points_at_its_own_ancestor_terminates_with_a_warning() {
    let fixture = Fixture::new();
    let loop_dir = fixture.home.join(".agents/skills/loop");
    skill_file(
      &loop_dir.join("real"),
      "SKILL.md",
      "real",
      "Found before the loop.",
    );
    std::os::unix::fs::symlink(&loop_dir, loop_dir.join("self")).expect("symlink");
    let scan = discover(&fixture.discovery());
    // Reaching a fixed point is the property; the warning is how the operator learns the
    // walk stopped rather than finding nothing.
    assert!(scan.named("real").is_some(), "{:?}", scan.skills);
    assert!(
      scan
        .warnings
        .iter()
        .any(|warning| matches!(warning, SkillWarning::TooDeep { .. })),
      "{:?}",
      scan.warnings
    );
  }
}
