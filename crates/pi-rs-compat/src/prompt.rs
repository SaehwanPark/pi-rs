//! Finding Pi's prompt templates, and expanding them the way Pi does.
//!
//! A prompt template is one Markdown file that becomes one command: the filename
//! without `.md` is the name, and typing `/name` sends the file's body to the model with
//! the caller's arguments substituted into it. The file is small and the rules are few,
//! which makes it the format most likely to be wrong in a reimplementation -- every
//! rule below is quoted from Pi's documented behavior, and nothing beyond it is invented.
//!
//! # Locations, and the order they are read in
//!
//! ```text
//! $HOME/.pi/agent/prompts/*.md
//! <ancestor>/.pi/prompts/*.md    project family, only when the project is trusted
//! ```
//!
//! Unlike skills there is no `.agents` family: Pi documents prompt templates under `.pi`
//! only. Ancestors run from `cwd` upward through the git root, nearest first, so a name
//! declared twice resolves the same way on every machine -- first found wins, and the
//! order is what makes "first" mean something.
//!
//! Package `prompts/` directories, `pi.prompts` entries in `package.json`, the `prompts`
//! array in settings, and `--prompt-template` paths are other sources in Pi and are not
//! implemented here yet.
//!
//! # What is quietly not a template
//!
//! Discovery is **non-recursive**: Pi says so, and says to add a subdirectory explicitly
//! through settings or a package manifest if you meant to include it. Files that are not
//! `.md` are not templates. Both of those are Pi skipping without comment, so they are
//! skipped here without comment too -- unlike everything else, which produces a
//! [`Warning`] naming the path and the reason.

use std::{fs, path::Path, path::PathBuf};

use crate::{
  frontmatter::{self, Extract},
  scan::{self, Source, Trust},
  substitute,
};

/// A template that was loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
  /// The filename without `.md`. Pi has no spelling rule for template names -- any
  /// filename is any command -- so none is enforced here either.
  pub name: String,
  pub description: String,
  /// Whether `description` is the frontmatter's or the body's first non-empty line. Pi
  /// falls back to the body; a surface that says "described as" must not credit the
  /// author with a line they wrote as prose.
  pub description_from_body: bool,
  /// `argument-hint`, verbatim. The `<required>` / `[optional]` brackets are a display
  /// convention that nothing here acts on yet.
  pub argument_hint: Option<String>,
  pub path: PathBuf,
  pub source: Source,
  /// The template text with the frontmatter removed and the surrounding blank lines
  /// taken off. The interior is untouched -- a template is prose, and its line breaks are
  /// part of it -- but the file's trailing newline is a fact about files, not about the
  /// prompt, so a surface that writes the expansion decides for itself how the output
  /// ends.
  pub body: String,
}

/// A candidate that was not loaded, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Warning {
  /// A `---` block that never closed. Not loaded: the rest of the file would be read as
  /// frontmatter, and the fallback description could then be picked up from anywhere.
  MalformedFrontmatter { path: PathBuf },
  /// No body. Not loaded: a template is a thing that expands into a prompt, and one
  /// that expands to nothing cannot be used, only be confused for a mistake.
  Empty { path: PathBuf },
  /// Two locations declared one name. The first found was kept, this one was dropped.
  Duplicate {
    name: String,
    kept: PathBuf,
    ignored: PathBuf,
  },
  /// The file exists and could not be read: permissions, or bytes that are not text.
  Unreadable { path: PathBuf, reason: String },
}

/// What a scan found, and everything it declined.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scan {
  pub templates: Vec<Template>,
  pub warnings: Vec<Warning>,
}

impl Scan {
  /// The kept template with this name, if one was loaded.
  pub fn named(&self, name: &str) -> Option<&Template> {
    self.templates.iter().find(|t| t.name == name)
  }
}

/// Scan the documented locations.
pub fn discover(discovery: &scan::Discovery) -> Scan {
  let mut scan = Scan::default();
  let mut names = Vec::<String>::new();
  if let Some(home) = &discovery.home {
    location(
      home.join(".pi/agent/prompts"),
      Source::Global,
      &mut scan,
      &mut names,
    );
  }
  if discovery.trust == Trust::Trusted {
    for project in scan::project_dirs(&discovery.cwd) {
      location(
        project.join(".pi/prompts"),
        Source::Project,
        &mut scan,
        &mut names,
      );
    }
  }
  scan
    .templates
    .sort_by(|a, b| a.source.cmp(&b.source).then(a.name.cmp(&b.name)));
  scan
}

/// One location, read non-recursively. Subdirectories and non-Markdown files are not
/// templates and Pi skips them without comment, so this does too.
fn location(root: PathBuf, source: Source, scan: &mut Scan, names: &mut Vec<String>) {
  let entries = match fs::read_dir(&root) {
    Ok(entries) => entries,
    Err(_) => return,
  };
  let mut files: Vec<PathBuf> = entries
    .flatten()
    .filter(|entry| {
      let path = entry.path();
      // Pi discovers templates as `*.md`, and a `*` does not match a leading dot, so
      // `.md` and `.draft.md` are not commands. Documented behavior, so nothing is said
      // about them on every run.
      let is_template_name = |name: &str| !name.starts_with('.') && name.ends_with(".md");
      path
        .file_name()
        .is_some_and(|name| is_template_name(&name.to_string_lossy()))
        && scan::is_file(&path.metadata().ok())
    })
    .map(|entry| entry.path())
    .collect();
  // Filesystem order decides which of two files claiming one name survives; sorting
  // makes that decision, and every test, reproducible.
  files.sort();
  for path in files {
    let name = path
      .file_stem()
      .map(|stem| stem.to_string_lossy().into_owned())
      .unwrap_or_default();
    let template = match load(&path, source, &name) {
      Ok(template) => template,
      Err(warning) => {
        scan.warnings.push(warning);
        continue;
      }
    };
    if names.contains(&template.name) {
      let kept = scan
        .templates
        .iter()
        .find(|kept| kept.name == template.name)
        .map(|kept| kept.path.clone())
        .unwrap_or_default();
      scan.warnings.push(Warning::Duplicate {
        name: template.name,
        kept,
        ignored: path,
      });
      continue;
    }
    names.push(template.name.clone());
    scan.templates.push(template);
  }
}

/// Read one file as a template.
fn load(path: &Path, source: Source, name: &str) -> Result<Template, Warning> {
  let text = fs::read_to_string(path).map_err(|error| Warning::Unreadable {
    path: path.to_path_buf(),
    reason: error.to_string(),
  })?;
  let frontmatter = match frontmatter::extract(&text) {
    // A template needs no frontmatter at all: the name is the filename and the
    // description may come from the body. This is the common case, not a defect.
    Extract::Absent => None,
    Extract::Malformed(_) => {
      return Err(Warning::MalformedFrontmatter {
        path: path.to_path_buf(),
      });
    }
    Extract::Found(frontmatter) => Some(frontmatter),
  };
  let body = frontmatter::strip(&text).trim().to_string();
  if body.is_empty() {
    return Err(Warning::Empty {
      path: path.to_path_buf(),
    });
  }
  let declared = frontmatter
    .as_ref()
    .and_then(|frontmatter| frontmatter.get("description"))
    .map(str::trim)
    .filter(|value| !value.is_empty())
    .map(str::to_string);
  let (description, description_from_body) = match declared {
    Some(description) => (description, false),
    // Pi: "If missing, the first non-empty line is used."
    None => (first_line(&body), true),
  };
  let argument_hint = frontmatter
    .as_ref()
    .and_then(|frontmatter| frontmatter.get("argument-hint"))
    .map(str::trim)
    .filter(|hint| !hint.is_empty())
    .map(str::to_string);
  Ok(Template {
    name: name.to_string(),
    description,
    description_from_body,
    argument_hint,
    path: path.to_path_buf(),
    source,
    body,
  })
}

impl Template {
  /// The prompt this template becomes with these arguments substituted.
  pub fn expand(&self, args: &[&str]) -> String {
    substitute::substitute(&self.body, args)
  }
}

/// The first non-empty line, trimmed. Used as the description when the file does not
/// declare one.
fn first_line(body: &str) -> String {
  body
    .lines()
    .map(str::trim)
    .find(|line| !line.is_empty())
    .unwrap_or_default()
    .to_string()
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::scan::Discovery;
  use tempfile::TempDir;

  struct Fixture {
    /// Held so the tree is removed when the test ends; nothing reads it.
    _root: TempDir,
    home: PathBuf,
    project: PathBuf,
  }

  impl Fixture {
    fn new() -> Self {
      let root = TempDir::new().expect("temp root");
      let home = root.path().join("home");
      let project = root.path().join("project");
      fs::create_dir_all(&home).expect("home");
      fs::create_dir_all(project.join(".git")).expect("project ceiling");
      Self {
        _root: root,
        home,
        project,
      }
    }

    /// Write one template into one of the two families.
    fn write(&self, global: bool, file: &str, contents: &str) {
      let dir = if global {
        self.home.join(".pi/agent/prompts")
      } else {
        self.project.join(".pi/prompts")
      };
      fs::create_dir_all(&dir).expect("prompts directory");
      fs::write(dir.join(file), contents).expect("write template");
    }

    fn discovery(&self, trust: Trust) -> Discovery {
      Discovery {
        home: Some(self.home.clone()),
        cwd: self.project.clone(),
        trust,
      }
    }

    fn scan(&self, trust: Trust) -> Scan {
      discover(&self.discovery(trust))
    }
  }

  #[test]
  fn a_template_is_a_file_whose_name_is_the_command() {
    let fixture = Fixture::new();
    fixture.write(
      true,
      "review.md",
      "---\ndescription: Review staged git changes\n---\nReview the staged changes.\n",
    );
    let scan = fixture.scan(Trust::Untrusted);
    let template = scan.named("review").expect("loaded review.md");
    assert_eq!(template.description, "Review staged git changes");
    assert!(!template.description_from_body);
    assert_eq!(template.source, Source::Global);
    assert_eq!(template.body, "Review the staged changes.");
    assert!(scan.warnings.is_empty(), "{:?}", scan.warnings);
  }

  #[test]
  fn the_project_family_is_read_only_when_the_project_is_trusted() {
    let fixture = Fixture::new();
    fixture.write(
      true,
      "global.md",
      "---\ndescription: home\n---\nhome body\n",
    );
    fixture.write(
      false,
      "deploy.md",
      "---\ndescription: deploy\n---\ndeploy body\n",
    );
    let untrusted = fixture.scan(Trust::Untrusted);
    let names: Vec<&str> = untrusted
      .templates
      .iter()
      .map(|template| template.name.as_str())
      .collect();
    assert_eq!(names, ["global"]);
    let trusted = fixture.scan(Trust::Trusted);
    let trusted_names: Vec<&str> = trusted
      .templates
      .iter()
      .map(|template| template.name.as_str())
      .collect();
    assert_eq!(trusted_names, ["global", "deploy"]);
    assert_eq!(
      trusted.named("deploy").expect("deploy").source,
      Source::Project
    );
  }

  #[test]
  fn a_missing_description_is_the_first_line_literally() {
    let fixture = Fixture::new();
    fixture.write(true, "plain.md", "\n\nFirst real line.\n\nLater prose.\n");
    let scan = fixture.scan(Trust::Untrusted);
    let template = scan.named("plain").expect("a body alone is a template");
    assert_eq!(template.description, "First real line.");
    assert!(template.description_from_body);
    // Pi says "the first non-empty line", not "the first line that reads well", and does
    // not strip Markdown on the way. A template that opens with a heading therefore
    // advertises the heading -- the reason to write a `description` rather than lean on
    // the fallback.
    fixture.write(true, "headed.md", "# Head\n\nSecond line.\n");
    let scan = fixture.scan(Trust::Untrusted);
    let headed = scan.named("headed").expect("headed.md");
    assert_eq!(headed.description, "# Head");
    assert!(headed.description_from_body);
  }

  #[test]
  fn an_argument_hint_is_kept_as_written() {
    let fixture = Fixture::new();
    fixture.write(
      true,
      "pr.md",
      "---\ndescription: Review PRs\nargument-hint: \"<PR-URL>\"\n---\nbody\n",
    );
    let scan = fixture.scan(Trust::Untrusted);
    let template = scan.named("pr").expect("pr.md");
    assert_eq!(template.argument_hint.as_deref(), Some("<PR-URL>"));
    assert_eq!(template.expand(&[]), "body");
  }

  #[test]
  fn discovery_is_not_recursive_and_says_nothing_about_the_files_pi_skips_quietly() {
    let fixture = Fixture::new();
    fixture.write(true, "top.md", "---\ndescription: top\n---\nbody\n");
    fixture.write(true, "notes.txt", "not markdown\n");
    let nested = fixture.home.join(".pi/agent/prompts/nested");
    fs::create_dir_all(&nested).expect("nested");
    fs::write(
      nested.join("inner.md"),
      "---\ndescription: inner\n---\nbody\n",
    )
    .expect("inner");
    let scan = fixture.scan(Trust::Untrusted);
    let names: Vec<&str> = scan
      .templates
      .iter()
      .map(|template| template.name.as_str())
      .collect();
    assert_eq!(names, ["top"]);
    // Pi documents non-recursion as the rule, and says to add a subdirectory through
    // settings when you meant otherwise. A rule the user can look up does not need to be
    // repeated every run.
    assert!(scan.warnings.is_empty(), "{:?}", scan.warnings);
  }

  #[test]
  fn a_file_that_cannot_become_a_prompt_is_named_and_skipped() {
    let fixture = Fixture::new();
    fixture.write(
      true,
      "unclosed.md",
      "---\ndescription: never closed\nstill frontmatter\n",
    );
    fixture.write(
      true,
      "empty.md",
      "---\ndescription: nothing to expand\n---\n\n",
    );
    let scan = fixture.scan(Trust::Untrusted);
    assert!(scan.templates.is_empty(), "{:?}", scan.templates);
    assert_eq!(scan.warnings.len(), 2, "{:?}", scan.warnings);
    let described: Vec<String> = scan
      .warnings
      .iter()
      .map(|warning| match warning {
        Warning::MalformedFrontmatter { path } => {
          format!("malformed:{}", path.file_name().unwrap().to_string_lossy())
        }
        Warning::Empty { path } => {
          format!("empty:{}", path.file_name().unwrap().to_string_lossy())
        }
        Warning::Unreadable { path, .. } => {
          format!("unreadable:{}", path.file_name().unwrap().to_string_lossy())
        }
        Warning::Duplicate { name, .. } => format!("duplicate:{name}"),
      })
      .collect();
    // Sorted by filename, so the empty one is reported first.
    assert_eq!(described, ["empty:empty.md", "malformed:unclosed.md"]);
  }

  #[test]
  fn a_dotfile_is_not_matched_by_the_rule_that_finds_templates() {
    let fixture = Fixture::new();
    // Pi looks for `*.md`, and `*` does not match a leading dot. These are not commands,
    // and no warning claims otherwise on every run.
    fixture.write(true, ".md", "---\ndescription: no name\n---\nbody\n");
    fixture.write(true, ".draft.md", "---\ndescription: draft\n---\nbody\n");
    let scan = fixture.scan(Trust::Untrusted);
    assert!(scan.templates.is_empty(), "{:?}", scan.templates);
    assert!(scan.warnings.is_empty(), "{:?}", scan.warnings);
  }

  #[test]
  fn the_first_location_to_declare_a_name_keeps_it() {
    let fixture = Fixture::new();
    fixture.write(
      true,
      "dup.md",
      "---\ndescription: home copy\n---\nhome body\n",
    );
    fixture.write(
      false,
      "dup.md",
      "---\ndescription: project copy\n---\nproject body\n",
    );
    let scan = fixture.scan(Trust::Trusted);
    let dup = scan.named("dup").expect("one dup kept");
    assert_eq!(dup.description, "home copy");
    assert!(
      scan
        .warnings
        .iter()
        .any(|warning| matches!(warning, Warning::Duplicate { name, .. } if name == "dup")),
      "{:?}",
      scan.warnings
    );
  }

  #[test]
  fn expansion_at_the_surface_is_expansion_in_the_library() {
    let fixture = Fixture::new();
    fixture.write(
      true,
      "component.md",
      "---\ndescription: Create a component\n---\nCreate a React component named $1 with features: $@\n",
    );
    let scan = fixture.scan(Trust::Untrusted);
    let template = scan.named("component").expect("component");
    assert_eq!(
      template.expand(&["Button", "onClick"]),
      "Create a React component named Button with features: Button onClick"
    );
  }
}
