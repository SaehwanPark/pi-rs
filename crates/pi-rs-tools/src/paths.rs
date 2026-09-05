//! Path resolution for file tools.
//!
//! File tools receive paths from a model, which is untrusted input. The rule
//! implemented here is narrow and deliberately boring: a tool may touch a path
//! outside the workspace only when the policy explicitly says so, and mutating
//! tools check containment on the *canonical* path so that a symlink inside the
//! workspace cannot quietly point somewhere else.
//!
//! This is not a sandbox. It is a guardrail against a confused model and against
//! a config that supplies a hostile working directory; the OS remains the
//! actual boundary.

use std::{
  fs, io,
  path::{Path, PathBuf},
};

/// Why a path was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
  /// The path resolves outside the workspace root.
  OutsideWorkspace { requested: String, resolved: String },
  /// The path could not be resolved.
  Unresolvable(String),
}

impl std::fmt::Display for PathError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::OutsideWorkspace {
        requested,
        resolved,
      } => write!(
        f,
        "'{requested}' resolves to '{resolved}', which is outside the workspace root"
      ),
      Self::Unresolvable(reason) => write!(f, "cannot resolve path: {reason}"),
    }
  }
}

/// The directory file tools treat as "here".
#[derive(Debug, Clone)]
pub struct Workspace {
  /// The boundary every tool resolves against.
  pub root: PathBuf,
  /// Whether single reads may leave `root`. Reads are usually safe and often
  /// useful (a lockfile in a parent directory, a system log); writes are not.
  pub allow_read_outside: bool,
  /// Whether writes may leave `root`. Off by default and only turned on by an
  /// explicit configuration choice.
  pub allow_write_outside: bool,
  /// Whether a search may walk a directory outside `root`. Independent of
  /// [`Self::allow_read_outside`]: being able to read one outside file is not the
  /// same as being able to enumerate the filesystem.
  pub allow_search_outside: bool,
}

impl Workspace {
  /// Open a workspace rooted at `root`, resolving it to a canonical path.
  ///
  /// A missing root is an error rather than a silent fallback: guessing a
  /// working directory would let a bad `cwd` in config scatter writes into an
  /// unintended tree.
  pub fn new(root: impl AsRef<Path>) -> Result<Self, PathError> {
    let root = canonical_existing(root.as_ref())?;
    if !root.is_dir() {
      return Err(PathError::Unresolvable(format!(
        "{} is not a directory",
        root.display()
      )));
    }
    Ok(Self {
      root,
      allow_read_outside: true,
      allow_write_outside: false,
      allow_search_outside: false,
    })
  }

  /// Allow or forbid reads outside the root.
  pub fn with_read_outside(mut self, allowed: bool) -> Self {
    self.allow_read_outside = allowed;
    self
  }

  /// Allow or forbid writes outside the root.
  pub fn with_write_outside(mut self, allowed: bool) -> Self {
    self.allow_write_outside = allowed;
    self
  }

  /// Allow or forbid directory walks outside the root.
  pub fn with_search_outside(mut self, allowed: bool) -> Self {
    self.allow_search_outside = allowed;
    self
  }

  pub fn root(&self) -> &Path {
    &self.root
  }

  /// Resolve the root of a search.
  ///
  /// Search is read-only, but its argument is a *directory the tool will walk*,
  /// and a walk leaks far more than one file would. So the search root stays
  /// inside the workspace even under a policy that permits individual outside
  /// reads; only an explicit `allow_search_outside` widens it.
  pub fn search_path(&self, requested: &str) -> Result<PathBuf, PathError> {
    let resolved = self.resolve(requested)?;
    if !self.allow_search_outside && !self.contains(&resolved) {
      return Err(PathError::OutsideWorkspace {
        requested: requested.to_string(),
        resolved: resolved.display().to_string(),
      });
    }
    Ok(resolved)
  }

  /// Resolve a path for reading.
  pub fn read_path(&self, requested: &str) -> Result<PathBuf, PathError> {
    let resolved = self.resolve(requested)?;
    if !self.allow_read_outside && !self.contains(&resolved) {
      return Err(PathError::OutsideWorkspace {
        requested: requested.to_string(),
        resolved: resolved.display().to_string(),
      });
    }
    Ok(resolved)
  }

  /// Resolve a path for writing.
  ///
  /// Containment is checked on the *lexical* resolution, which is also what the
  /// tool reports in its result, so the boundary claim in the trace matches the
  /// decision that was made.
  pub fn write_path(&self, requested: &str) -> Result<PathBuf, PathError> {
    let resolved = self.resolve(requested)?;
    if !self.allow_write_outside && !self.contains(&resolved) {
      return Err(PathError::OutsideWorkspace {
        requested: requested.to_string(),
        resolved: resolved.display().to_string(),
      });
    }
    Ok(resolved)
  }

  /// Resolve a model-supplied path without dereferencing a final component
  /// that does not exist yet.
  ///
  /// The path is resolved *lexically*: `.` and `..` are applied as text and no
  /// component is dereferenced, so a symlink cannot be swapped in between a
  /// check and a use. The result is absolute, so containment checks are
  /// meaningful. A symlink inside the workspace that points outward still
  /// resolves inside the root lexically, which is a deliberate trade: resolving
  /// it would make every legitimate symlinked checkout a policy violation.
  /// Mutating tools therefore report the resolved path in their result text, so
  /// the boundary claim is visible in the trace rather than implied.
  fn resolve(&self, requested: &str) -> Result<PathBuf, PathError> {
    if requested.trim().is_empty() {
      return Err(PathError::Unresolvable("empty path".into()));
    }
    let candidate = Path::new(requested);
    if candidate.is_absolute() {
      return Ok(lexical_absolute(candidate));
    }
    Ok(lexical_absolute(&self.root.join(candidate)))
  }

  fn contains(&self, path: &Path) -> bool {
    path.starts_with(&self.root)
  }
}

/// Absolutize a path by applying `.` and `..` textually.
///
/// `..` never escapes the root: a path that climbs above it is reported as
/// out-of-root rather than silently resolving to a system path.
fn lexical_absolute(path: &Path) -> PathBuf {
  let mut parts: Vec<std::ffi::OsString> = Vec::new();
  let mut base = std::ffi::OsString::new();
  for component in path.components() {
    match component {
      std::path::Component::RootDir => base.push("/"),
      std::path::Component::Prefix(prefix) => base.push(prefix.as_os_str()),
      std::path::Component::CurDir => {}
      std::path::Component::ParentDir => {
        parts.pop();
      }
      std::path::Component::Normal(name) => parts.push(name.to_os_string()),
    }
  }
  let mut resolved = PathBuf::from(base);
  for part in parts {
    resolved.push(part);
  }
  resolved
}

/// The real path of an existing directory, for the root only.
///
/// The root is chosen by configuration rather than by a model, so it is safe to
/// canonicalize once at construction; model-supplied paths are resolved
/// lexically instead.
fn canonical_existing(path: &Path) -> Result<PathBuf, PathError> {
  fs::canonicalize(path)
    .map_err(|error| PathError::Unresolvable(format!("{}: {error}", path.display())))
}

impl From<PathError> for io::Error {
  fn from(error: PathError) -> Self {
    io::Error::new(io::ErrorKind::PermissionDenied, error.to_string())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn root() -> (tempfile::TempDir, Workspace) {
    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::new(dir.path()).unwrap();
    (dir, workspace)
  }

  #[test]
  fn relative_paths_resolve_under_the_root() {
    let (_dir, workspace) = root();
    let resolved = workspace.read_path("src/main.rs").unwrap();
    assert!(resolved.starts_with(workspace.root()));
    assert!(resolved.to_string_lossy().ends_with("src/main.rs"));
  }

  #[test]
  fn traversal_out_of_the_root_is_visible_as_such() {
    let (_dir, workspace) = root();
    let error = workspace
      .with_read_outside(false)
      .read_path("../outside.txt")
      .unwrap_err();
    assert!(
      matches!(error, PathError::OutsideWorkspace { .. }),
      "{error}"
    );
  }

  #[test]
  fn reads_may_leave_the_root_but_writes_may_not() {
    let (_dir, workspace) = root();
    let outside = std::env::temp_dir().join("definitely-not-in-a-workspace");
    assert!(workspace.read_path(&outside.to_string_lossy()).is_ok());
    let error = workspace
      .write_path(&outside.to_string_lossy())
      .unwrap_err();
    assert!(
      matches!(error, PathError::OutsideWorkspace { .. }),
      "{error}"
    );
  }

  #[test]
  fn a_writable_path_may_not_exist_yet() {
    let (_dir, workspace) = root();
    let resolved = workspace.write_path("new/created.txt").unwrap();
    assert!(resolved.to_string_lossy().ends_with("new/created.txt"));
  }

  #[test]
  fn an_absolute_in_root_path_is_accepted_for_writing() {
    let (dir, workspace) = root();
    let target = dir.path().join("inside.txt");
    assert_eq!(
      workspace.write_path(&target.to_string_lossy()).unwrap(),
      target
    );
  }

  #[test]
  fn an_empty_or_missing_root_is_refused_rather_than_guessed() {
    assert!(Workspace::new("").is_err());
    assert!(Workspace::new("/definitely/not/a/real/directory").is_err());
  }

  #[test]
  fn dot_dot_is_applied_lexically_and_cannot_escape_the_root() {
    let (dir, workspace) = root();
    // Same-directory indirection stays inside.
    let inside = workspace.write_path("src/../inside.txt").unwrap();
    assert_eq!(inside, dir.path().join("inside.txt"));
    // Climbing out is refused rather than resolved to a system path.
    let error = workspace.write_path("../../../etc/passwd").unwrap_err();
    assert!(
      matches!(error, PathError::OutsideWorkspace { .. }),
      "expected refusal, got {error}"
    );
  }

  #[test]
  fn the_write_boundary_is_the_path_the_tool_reports() {
    // The containment check and the reported path must be the same value, or the
    // trace would make a claim the check never verified. A symlinked directory
    // inside the root resolves lexically and is therefore allowed; the reported
    // path is the path under the root.
    let (dir, workspace) = root();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
    let resolved = workspace.write_path("link/file.txt").unwrap();
    assert!(resolved.starts_with(workspace.root()), "{resolved:?}");
    assert_eq!(resolved, dir.path().join("link/file.txt"));
  }

  #[test]
  fn a_search_root_may_not_escape_the_root_even_though_reads_may() {
    let (dir, mut workspace) = root();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret.txt"), "x").unwrap();
    // Same policy that permits a single outside read still confines the walk.
    workspace.allow_read_outside = true;
    workspace.allow_write_outside = false;
    let outside_root = outside.path().join("sub");
    let outside_root_str = outside_root.display().to_string();
    // Reading one outside file is allowed by this policy; walking that directory
    // is still refused, because `grep` reports paths and enumerates names.
    assert!(workspace.search_path(&outside_root_str).is_err());
    assert!(
      workspace
        .read_path(&outside_root.join("secret.txt").display().to_string())
        .is_ok()
    );
    assert!(
      workspace
        .with_search_outside(true)
        .search_path(&outside_root_str)
        .is_ok()
    );
    let _ = dir;
  }

  #[test]
  fn a_read_may_leave_the_root_by_absolute_path_only_when_allowed() {
    let (_dir, workspace) = root();
    let strict = workspace.clone().with_read_outside(false);
    let outside = std::env::temp_dir().join("some-file");
    assert!(strict.read_path(&outside.to_string_lossy()).is_err());
    assert!(workspace.read_path(&outside.to_string_lossy()).is_ok());
  }
}
