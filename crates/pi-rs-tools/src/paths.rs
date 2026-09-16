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
  /// Whether single reads may leave `root`. This is disabled by default because
  /// model-visible file content is an egress boundary; callers must opt in with
  /// an explicit policy flag when a broader read is intended.
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
      allow_read_outside: false,
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
    if !self.allow_search_outside {
      self.check_confined(requested, &resolved)?;
    }
    Ok(resolved)
  }

  /// Resolve a path for reading.
  pub fn read_path(&self, requested: &str) -> Result<PathBuf, PathError> {
    let resolved = self.resolve(requested)?;
    if !self.allow_read_outside {
      self.check_confined(requested, &resolved)?;
    }
    Ok(resolved)
  }

  /// Resolve a path for writing.
  ///
  /// Containment is checked before the operation and every existing component
  /// is rejected when it is a symlink. The lexical path is still returned so
  /// result text and policy diagnostics name the path the tool was asked to use.
  pub fn write_path(&self, requested: &str) -> Result<PathBuf, PathError> {
    let resolved = self.resolve(requested)?;
    if !self.allow_write_outside {
      self.check_confined(requested, &resolved)?;
    }
    Ok(resolved)
  }

  /// Resolve a model-supplied path without requiring a final component to exist.
  ///
  /// The lexical pass applies `.` and `..` as text and produces an absolute path.
  /// Confined callers then run [`Self::check_confined`], which inspects every
  /// existing component and rejects symlink traversal before handing the path to
  /// a filesystem operation.
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

  /// Verify the path without allowing an existing symlink to redirect a file
  /// operation. Missing final components are valid for `write`, so validation
  /// walks only what exists and stops at the first missing component.
  fn check_confined(&self, requested: &str, resolved: &Path) -> Result<(), PathError> {
    if !self.contains(resolved) {
      return Err(PathError::OutsideWorkspace {
        requested: requested.to_string(),
        resolved: resolved.display().to_string(),
      });
    }

    let relative = resolved.strip_prefix(&self.root).map_err(|_| {
      PathError::Unresolvable(format!("{} is not under the workspace", resolved.display()))
    })?;
    let mut current = self.root.clone();
    for component in relative.components() {
      let std::path::Component::Normal(name) = component else {
        continue;
      };
      current.push(name);
      match fs::symlink_metadata(&current) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
          let target = fs::canonicalize(&current).ok();
          if let Some(target) = &target
            && !self.contains(target)
          {
            return Err(PathError::OutsideWorkspace {
              requested: requested.to_string(),
              resolved: target.display().to_string(),
            });
          }
          return Err(PathError::Unresolvable(format!(
            "symlink component '{}' is not allowed in a confined path",
            current.display()
          )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => break,
        Err(error) => {
          return Err(PathError::Unresolvable(format!(
            "cannot inspect '{}': {error}",
            current.display()
          )));
        }
      }
    }

    // Re-check an existing target through the OS resolver. This catches a
    // symlink introduced between the component walk and the operation in the
    // common (non-racing) case, and ensures final links cannot escape.
    match fs::canonicalize(resolved) {
      Ok(canonical) if !self.contains(&canonical) => Err(PathError::OutsideWorkspace {
        requested: requested.to_string(),
        resolved: canonical.display().to_string(),
      }),
      Ok(_) => Ok(()),
      Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
      Err(error) => Err(PathError::Unresolvable(format!(
        "cannot resolve '{}': {error}",
        resolved.display()
      ))),
    }
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

  fn slash_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
  }

  #[test]
  fn relative_paths_resolve_under_the_root() {
    let (_dir, workspace) = root();
    let resolved = workspace.read_path("src/main.rs").unwrap();
    assert!(resolved.starts_with(workspace.root()));
    assert!(slash_path(&resolved).ends_with("src/main.rs"));
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
  fn reads_and_writes_are_confined_by_default() {
    let (_dir, workspace) = root();
    let outside = std::env::temp_dir().join("definitely-not-in-a-workspace");
    assert!(workspace.read_path(&outside.to_string_lossy()).is_err());
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
    assert!(slash_path(&resolved).ends_with("new/created.txt"));
  }

  #[test]
  fn an_absolute_in_root_path_is_accepted_for_writing() {
    let (_dir, workspace) = root();
    let target = workspace.root().join("inside.txt");
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
    let (_dir, workspace) = root();
    // Same-directory indirection stays inside.
    let inside = workspace.write_path("src/../inside.txt").unwrap();
    assert_eq!(inside, workspace.root().join("inside.txt"));
    // Climbing out is refused rather than resolved to a system path.
    let error = workspace.write_path("../../../etc/passwd").unwrap_err();
    assert!(
      matches!(error, PathError::OutsideWorkspace { .. }),
      "expected refusal, got {error}"
    );
  }

  #[cfg(unix)]
  #[test]
  fn outward_parent_symlink_is_refused_for_confined_paths() {
    let (_dir, workspace) = root();
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), workspace.root().join("link")).unwrap();
    for operation in [
      workspace.write_path("link/file.txt"),
      workspace.read_path("link/file.txt"),
      workspace.search_path("link"),
    ] {
      let error = operation.unwrap_err();
      assert!(
        matches!(error, PathError::OutsideWorkspace { .. }),
        "{error}"
      );
    }
  }

  #[cfg(unix)]
  #[test]
  fn outward_final_symlink_is_refused_for_confined_paths() {
    let (_dir, workspace) = root();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("secret.txt");
    fs::write(&target, "secret").unwrap();
    std::os::unix::fs::symlink(&target, workspace.root().join("secret.txt")).unwrap();
    let error = workspace.write_path("secret.txt").unwrap_err();
    assert!(
      matches!(error, PathError::OutsideWorkspace { .. }),
      "{error}"
    );
    let error = workspace.read_path("secret.txt").unwrap_err();
    assert!(
      matches!(error, PathError::OutsideWorkspace { .. }),
      "{error}"
    );
  }

  #[test]
  fn a_search_root_may_not_escape_the_root_even_though_reads_may() {
    let (_dir, mut workspace) = root();
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
  }

  #[test]
  fn a_read_may_leave_the_root_by_absolute_path_only_when_allowed() {
    let (_dir, workspace) = root();
    let strict = workspace.clone().with_read_outside(false);
    let outside = std::env::temp_dir().join("some-file");
    assert!(strict.read_path(&outside.to_string_lossy()).is_err());
    assert!(
      workspace
        .with_read_outside(true)
        .read_path(&outside.to_string_lossy())
        .is_ok()
    );
  }
}
