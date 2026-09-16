//! Durable project-trust decisions.
//!
//! Trust is configuration with security consequences, not a side effect of reading a
//! project file. This module stores only decisions made by an explicit caller and loads
//! them before any project-local behavior is activated. A missing file means no grants;
//! malformed or newer files fail closed so a damaged trust record cannot become approval.

use std::{
  fs::{self, File, OpenOptions},
  io::Write,
  path::{Path, PathBuf},
  process,
  time::{SystemTime, UNIX_EPOCH},
};

use pi_rs_core::trust::{TrustEntry, TrustScope, TrustStore};
use serde::{Deserialize, Serialize};

use crate::StoreError;

/// Schema version of the durable trust file.
pub const TRUST_SCHEMA_VERSION: u32 = 1;
const TRUST_FILE_NAME: &str = "trust.json";

#[derive(Debug, Serialize, Deserialize)]
struct TrustFile {
  version: u32,
  #[serde(default)]
  entries: Vec<TrustEntry>,
}

/// A loaded, schema-checked trust file.
#[derive(Debug, Clone)]
pub struct FileTrustStore {
  path: PathBuf,
  entries: Vec<TrustEntry>,
}

impl FileTrustStore {
  /// Open `<root>/trust.json`, treating a missing file as an empty store.
  pub fn open(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
    let root = root.into();
    create_private_dir(&root)?;
    let path = root.join(TRUST_FILE_NAME);
    let entries = read_entries(&path)?;
    if fs::symlink_metadata(&path).is_ok() {
      set_private_path(&path)?;
    }
    Ok(Self { path, entries })
  }

  /// The durable path, useful for diagnostics without exposing file contents.
  pub fn path(&self) -> &Path {
    &self.path
  }

  /// Return all recorded decisions in stable scope-key order.
  pub fn entries(&self) -> Vec<TrustEntry> {
    let mut entries = self.entries.clone();
    entries.sort_by_key(|entry| entry.scope.key());
    entries
  }

  /// Look up a decision without touching the filesystem.
  pub fn lookup_entry(&self, scope: &TrustScope) -> Option<TrustEntry> {
    self
      .entries
      .iter()
      .find(|entry| entry.scope == *scope)
      .cloned()
  }

  /// Remove one exact scope, persisting the change if it existed.
  pub fn remove(&mut self, scope: &TrustScope) -> Result<bool, StoreError> {
    let previous = self.entries.clone();
    self.entries.retain(|entry| entry.scope != *scope);
    if self.entries.len() == previous.len() {
      return Ok(false);
    }
    if let Err(error) = self.persist() {
      self.entries = previous;
      return Err(error);
    }
    Ok(true)
  }

  fn persist(&self) -> Result<(), StoreError> {
    let parent = self
      .path
      .parent()
      .ok_or_else(|| StoreError::Invalid("trust path has no parent directory".into()))?;
    create_private_dir(parent)?;
    let bytes = serde_json::to_vec_pretty(&TrustFile {
      version: TRUST_SCHEMA_VERSION,
      entries: self.entries.clone(),
    })?;
    let temp = temporary_path(&self.path);
    let result = (|| {
      let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)?;
      set_private_file(&file)?;
      file.write_all(&bytes)?;
      file.write_all(b"\n")?;
      file.sync_all()?;
      drop(file);
      fs::rename(&temp, &self.path)?;
      sync_parent_dir(parent)?;
      Ok::<(), StoreError>(())
    })();
    if result.is_err() {
      let _ = fs::remove_file(&temp);
    }
    result
  }
}

impl TrustStore for FileTrustStore {
  fn lookup(&self, scope: &TrustScope) -> Option<TrustEntry> {
    self.lookup_entry(scope)
  }

  fn record(&mut self, entry: TrustEntry) -> Result<(), String> {
    let previous = self.entries.clone();
    if let Some(existing) = self
      .entries
      .iter_mut()
      .find(|existing| existing.scope == entry.scope)
    {
      *existing = entry;
    } else {
      self.entries.push(entry);
    }
    if let Err(error) = self.persist() {
      self.entries = previous;
      return Err(error.to_string());
    }
    Ok(())
  }
}

fn read_entries(path: &Path) -> Result<Vec<TrustEntry>, StoreError> {
  let metadata = match fs::symlink_metadata(path) {
    Ok(metadata) => metadata,
    Err(error) if StoreError::is_missing(&error) => return Ok(Vec::new()),
    Err(error) => return Err(StoreError::Io(error)),
  };
  if metadata.file_type().is_symlink() {
    return Err(StoreError::Invalid(format!(
      "trust file {} must not be a symlink",
      path.display()
    )));
  }
  let bytes = fs::read(path)?;
  let file: TrustFile = serde_json::from_slice(&bytes).map_err(|error| {
    StoreError::Invalid(format!(
      "cannot decode trust file {}: {error}",
      path.display()
    ))
  })?;
  if file.version != TRUST_SCHEMA_VERSION {
    return Err(StoreError::Invalid(format!(
      "trust file {} has unsupported schema version {} (expected {})",
      path.display(),
      file.version,
      TRUST_SCHEMA_VERSION
    )));
  }
  let mut seen = std::collections::BTreeSet::new();
  for entry in &file.entries {
    if !seen.insert(entry.scope.key()) {
      return Err(StoreError::Invalid(format!(
        "trust file {} contains duplicate scope {}",
        path.display(),
        entry.scope.key()
      )));
    }
  }
  Ok(file.entries)
}

fn temporary_path(path: &Path) -> PathBuf {
  let stamp = SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|duration| duration.as_nanos())
    .unwrap_or_default();
  PathBuf::from(format!("{}.tmp-{}-{stamp}", path.display(), process::id()))
}

fn create_private_dir(path: &Path) -> Result<(), StoreError> {
  fs::create_dir_all(path)?;
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
  }
  Ok(())
}

fn sync_parent_dir(path: &Path) -> Result<(), StoreError> {
  #[cfg(unix)]
  {
    File::open(path)?.sync_all()?;
  }
  #[cfg(not(unix))]
  {
    let _ = path;
  }
  Ok(())
}

fn set_private_file(file: &File) -> Result<(), StoreError> {
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
  }
  #[cfg(not(unix))]
  let _ = file;
  Ok(())
}

fn set_private_path(path: &Path) -> Result<(), StoreError> {
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
  }
  #[cfg(not(unix))]
  let _ = path;
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use pi_rs_core::trust::{Risk, TrustDecision, TrustGate, TrustOutcome};

  fn scope(name: &str) -> TrustScope {
    TrustScope::Project {
      root: format!("/tmp/{name}"),
    }
  }

  #[test]
  fn missing_file_is_empty_and_record_round_trips() {
    let temp = tempfile::TempDir::new().unwrap();
    let mut store = FileTrustStore::open(temp.path()).unwrap();
    let entry = TrustEntry::new(scope("repo"), TrustDecision::Trusted, "test");
    store.record(entry.clone()).unwrap();
    let reopened = FileTrustStore::open(temp.path()).unwrap();
    assert_eq!(reopened.lookup(&scope("repo")), Some(entry));
    assert_eq!(reopened.entries().len(), 1);
    assert_eq!(
      TrustGate::new().check(&reopened, &scope("repo"), Risk::High, false),
      TrustOutcome::Allowed
    );
  }

  #[test]
  fn recording_same_scope_replaces_the_previous_decision() {
    let temp = tempfile::TempDir::new().unwrap();
    let mut store = FileTrustStore::open(temp.path()).unwrap();
    store
      .record(TrustEntry::new(
        scope("repo"),
        TrustDecision::Trusted,
        "first",
      ))
      .unwrap();
    store
      .record(TrustEntry::new(
        scope("repo"),
        TrustDecision::Denied,
        "second",
      ))
      .unwrap();
    let entries = store.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].decision, TrustDecision::Denied);
    assert_eq!(entries[0].source, "second");
  }

  #[test]
  fn malformed_and_newer_files_fail_closed() {
    let temp = tempfile::TempDir::new().unwrap();
    let path = temp.path().join(TRUST_FILE_NAME);
    fs::write(&path, b"not-json").unwrap();
    assert!(FileTrustStore::open(temp.path()).is_err());
    fs::write(&path, br#"{"version":99,"entries":[]}"#).unwrap();
    assert!(FileTrustStore::open(temp.path()).is_err());
  }

  #[test]
  fn duplicate_scopes_are_rejected_instead_of_guessing() {
    let temp = tempfile::TempDir::new().unwrap();
    let entry = TrustEntry::new(scope("repo"), TrustDecision::Trusted, "test");
    let file = TrustFile {
      version: TRUST_SCHEMA_VERSION,
      entries: vec![entry.clone(), entry],
    };
    fs::write(
      temp.path().join(TRUST_FILE_NAME),
      serde_json::to_vec(&file).unwrap(),
    )
    .unwrap();
    assert!(FileTrustStore::open(temp.path()).is_err());
  }

  #[cfg(unix)]
  #[test]
  fn trust_state_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::TempDir::new().unwrap();
    let mut store = FileTrustStore::open(temp.path()).unwrap();
    store
      .record(TrustEntry::new(
        scope("repo"),
        TrustDecision::Trusted,
        "test",
      ))
      .unwrap();
    assert_eq!(
      fs::metadata(temp.path()).unwrap().permissions().mode() & 0o777,
      0o700
    );
    assert_eq!(
      fs::metadata(store.path()).unwrap().permissions().mode() & 0o777,
      0o600
    );
  }
}
