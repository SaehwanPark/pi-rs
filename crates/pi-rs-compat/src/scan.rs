//! Where to look, and under what assumptions.
//!
//! Skills, prompt templates, and any later file format all come from the same handful
//! of places -- under `$HOME`, or inside a project the operator has trusted -- and they
//! all arrive as observations about files on disk. The types that describe *where* and
//! *whether* therefore live once, here, rather than being reinvented per format with
//! slightly different rules. A trust decision that exists twice is a trust decision
//! that can disagree with itself.

use std::{fs, path::Path, path::PathBuf};

/// Whether the project's own files may be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trust {
  /// The operator has accepted this project.
  Trusted,
  /// Nobody has. Project locations are skipped entirely.
  Untrusted,
}

/// Which kind of location a file came from.
///
/// Ordinal in display order: the user's own configuration first, then the project's.
/// Skills list in that order, and prompt templates sort by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
  /// Under `$HOME`: the user's own configuration, not something a repository supplied.
  Global,
  /// Under the working directory or an ancestor of it, including an inner repository
  /// inside a larger one.
  Project,
}

impl Source {
  /// Stable machine label.
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Global => "global",
      Self::Project => "project",
    }
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

/// The project roots whose locations are read: `cwd` upward through the git root, or to
/// the filesystem root when there is no repository.
pub(crate) fn project_dirs(cwd: &Path) -> Vec<PathBuf> {
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

pub(crate) fn is_dir(kind: &Option<fs::Metadata>) -> bool {
  kind.as_ref().is_some_and(fs::Metadata::is_dir)
}

pub(crate) fn is_file(kind: &Option<fs::Metadata>) -> bool {
  kind.as_ref().is_some_and(fs::Metadata::is_file)
}
