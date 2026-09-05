//! Ephemeral directories.
//!
//! Storage, tool, and runtime tests need a writable directory that is
//! guaranteed not to collide with a previous run, and retention tests need to
//! delete one. A dependency is not worth it for a unique temporary directory,
//! and a test-only crate would still be reachable from the startup path of
//! nothing at all: this helper is small, synchronous, and used by tests.

use std::{
  fs,
  path::{Path, PathBuf},
};

use pi_rs_core::ids::uuidv7;

/// Temporary directory removed on drop.
#[derive(Debug)]
pub struct TempDir {
  path: PathBuf,
}

impl TempDir {
  /// Create `<system temp>/pi-rs-<label>-<uuidv7>`.
  pub fn new(label: &str) -> Self {
    let safe: String = label
      .chars()
      .map(|ch| {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
          ch
        } else {
          '-'
        }
      })
      .collect();
    let path = std::env::temp_dir().join(format!("pi-rs-{safe}-{}", uuidv7()));
    fs::create_dir_all(&path).expect("create temporary directory");
    Self { path }
  }

  pub fn path(&self) -> &Path {
    &self.path
  }

  /// Path to a file inside the directory. The directory is created on demand.
  pub fn child(&self, name: &str) -> PathBuf {
    self.path.join(name)
  }
}

impl Drop for TempDir {
  fn drop(&mut self) {
    let _ = fs::remove_dir_all(&self.path);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn directories_are_unique_and_removed_on_drop() {
    let first = TempDir::new("tmp-a");
    let second = TempDir::new("tmp-a");
    assert_ne!(first.path(), second.path());
    assert!(first.path().is_dir());
    let leaked = second.path().to_path_buf();
    drop(second);
    assert!(!leaked.exists(), "drop must clean up");
  }

  #[test]
  fn labels_cannot_create_path_structure() {
    let dir = TempDir::new("../../etc");
    assert!(dir.path().starts_with(std::env::temp_dir()));
    assert_eq!(dir.child("x").file_name().unwrap(), "x");
  }
}
