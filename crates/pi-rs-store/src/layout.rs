//! On-disk layout of the state directory.
//!
//! The layout is explicit and boring because several things depend on it:
//! session listing has to be cheap, blob references inside durable events must
//! stay resolvable after the surrounding event is reduced, and trace retention
//! has to be able to delete bounded regions without guessing.
//!
//! ```text
//! <state_dir>/sessions/<session-id>.jsonl            semantic session state
//! <state_dir>/sessions/<session-id>.trace.jsonl      high-resolution trace
//! <state_dir>/sessions/<session-id>/blobs/<xx>/<hash> content-addressed payloads
//! <state_dir>/sessions/<session-id>/checkpoints/<checkpoint-id>.json
//! <state_dir>/artifacts/                              cross-session artifacts
//! ```
//!
//! A session is therefore one file stem and one directory. That costs one
//! naming convention, and buys the property that deleting a session removes
//! exactly its state and nothing else.
//!
//! The state directory is created `0o700` where the platform supports modes:
//! durable traces may contain project content and credentials.

use std::{
  fs,
  path::{Path, PathBuf},
};

use pi_rs_core::{
  ids::{CheckpointId, SessionId},
  trace::BlobRef,
};

use crate::{StoreError, error::StoreError::Invalid};

const SESSIONS_DIR: &str = "sessions";
const ARTIFACTS_DIR: &str = "artifacts";
const BLOBS_DIR: &str = "blobs";
const CHECKPOINTS_DIR: &str = "checkpoints";
const SESSION_EXTENSION: &str = "jsonl";
const TRACE_SUFFIX: &str = ".trace.jsonl";

/// Where durable state for one runtime lives.
#[derive(Debug, Clone)]
pub struct StateLayout {
  root: PathBuf,
}

impl StateLayout {
  pub fn new(root: impl Into<PathBuf>) -> Self {
    Self { root: root.into() }
  }

  pub fn root(&self) -> &Path {
    &self.root
  }

  /// Create the root and its fixed subdirectories.
  ///
  /// Called once at store open, not per event, so that the startup path does a
  /// bounded amount of filesystem work.
  pub fn create(&self) -> Result<(), StoreError> {
    create_private_dir(&self.root)?;
    create_private_dir(&self.sessions_dir())?;
    create_private_dir(&self.artifacts_dir())
  }

  pub fn sessions_dir(&self) -> PathBuf {
    self.root.join(SESSIONS_DIR)
  }

  pub fn artifacts_dir(&self) -> PathBuf {
    self.root.join(ARTIFACTS_DIR)
  }

  /// `sessions/<id>.jsonl`.
  pub fn session_path(&self, session: &SessionId) -> PathBuf {
    self
      .sessions_dir()
      .join(format!("{session}.{SESSION_EXTENSION}"))
  }

  /// `sessions/<id>.trace.jsonl`.
  pub fn trace_path(&self, session: &SessionId) -> PathBuf {
    self.sessions_dir().join(format!("{session}{TRACE_SUFFIX}"))
  }

  /// `sessions/<id>/`, holding payloads and capsules.
  pub fn session_dir(&self, session: &SessionId) -> PathBuf {
    self.sessions_dir().join(session.as_str())
  }

  pub fn blobs_dir(&self, session: &SessionId) -> PathBuf {
    self.session_dir(session).join(BLOBS_DIR)
  }

  pub fn blob_path(&self, session: &SessionId, blob: &BlobRef) -> PathBuf {
    self
      .blobs_dir(session)
      .join(&blob.relative_path()["blobs/".len()..])
  }

  /// Directory holding a session's checkpoint capsules.
  pub fn checkpoints_dir(&self, session: &SessionId) -> PathBuf {
    self.session_dir(session).join(CHECKPOINTS_DIR)
  }

  /// Durable location of one checkpoint capsule.
  pub fn checkpoint_path(&self, session: &SessionId, checkpoint: &CheckpointId) -> PathBuf {
    self
      .session_dir(session)
      .join(CHECKPOINTS_DIR)
      .join(format!("{checkpoint}.json"))
  }

  /// Session-relative path recorded in durable state for one blob.
  pub fn blob_relative_path(&self, blob: &BlobRef) -> String {
    blob.relative_path()
  }

  pub fn ensure_session_dirs(&self, session: &SessionId) -> Result<(), StoreError> {
    create_private_dir(&self.session_dir(session))?;
    create_private_dir(&self.blobs_dir(session))?;
    create_private_dir(&self.session_dir(session).join(CHECKPOINTS_DIR))
  }

  /// Session ids that have a session file, newest first.
  ///
  /// Identifiers are time-ordered, so listing costs one directory read and one
  /// sort, with no file parsing. Callers that need metadata read headers only.
  pub fn list_session_ids(&self) -> Result<Vec<SessionId>, StoreError> {
    let mut ids = Vec::new();
    let dir = self.sessions_dir();
    if !dir.exists() {
      return Ok(ids);
    }
    for entry in fs::read_dir(&dir)? {
      let entry = entry?;
      let name = entry.file_name();
      let name = name.to_string_lossy().to_string();
      let Some(stem) = name.strip_suffix(&format!(".{SESSION_EXTENSION}")) else {
        continue;
      };
      // `*.trace.jsonl` also ends in the session extension: the stem would look
      // like `<id>.trace`, so both checks are needed to keep journals out of the
      // session list.
      if name.ends_with(TRACE_SUFFIX) || !is_session_id(stem) {
        continue;
      }
      ids.push(SessionId::from_string(stem.to_string()));
    }
    ids.sort_by(|a, b| b.as_str().cmp(a.as_str()));
    Ok(ids)
  }

  /// Total bytes held under `sessions/`, used by retention.
  ///
  /// This counts what actually occupies the directory, including per-session
  /// blob and checkpoint subdirectories and any file that is not a recognised
  /// session. A byte cap exists to bound disk usage, so a file this build does
  /// not understand must still count against it.
  pub fn state_bytes(&self) -> Result<u64, StoreError> {
    let mut total = 0u64;
    let dir = self.sessions_dir();
    if !dir.exists() {
      return Ok(0);
    }
    for entry in fs::read_dir(&dir)? {
      let entry = entry?;
      let file_type = entry.file_type()?;
      if file_type.is_file() {
        total += entry.metadata()?.len();
      } else if file_type.is_dir() {
        total += dir_bytes(&entry.path())?;
      }
    }
    Ok(total)
  }

  /// Size of one session's whole footprint.
  pub fn session_bytes(&self, session: &SessionId) -> Result<u64, StoreError> {
    let mut total = file_size(&self.session_path(session))? + file_size(&self.trace_path(session))?;
    let dir = self.session_dir(session);
    if dir.exists() {
      total += dir_bytes(&dir)?;
    }
    Ok(total)
  }

  /// Delete one session's files and directory.
  pub fn remove_session(&self, session: &SessionId) -> Result<(), StoreError> {
    remove_file_if_present(&self.session_path(session))?;
    remove_file_if_present(&self.trace_path(session))?;
    let dir = self.session_dir(session);
    if dir.exists() {
      fs::remove_dir_all(&dir)?;
    }
    Ok(())
  }

  /// Reject names that could escape the sessions directory.
  ///
  /// Session ids reach the filesystem in paths, and a caller may supply one
  /// read from an untrusted file, so containment is checked here rather than
  /// assumed.
  pub fn validate_session_id(session: &SessionId) -> Result<(), StoreError> {
    if is_session_id(session.as_str()) {
      Ok(())
    } else {
      Err(Invalid(format!(
        "session id {:?} is not a lowercase identifier",
        session.as_str()
      )))
    }
  }
}

fn is_session_id(stem: &str) -> bool {
  !stem.is_empty()
    && stem.len() <= 64
    && stem
      .chars()
      .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_')
}

fn create_private_dir(path: &Path) -> Result<(), StoreError> {
  fs::create_dir_all(path)?;
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    // Best-effort: a filesystem that does not support modes (for example some
    // network mounts) must not make the runtime unusable.
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
  }
  Ok(())
}

fn file_size(path: &Path) -> Result<u64, StoreError> {
  match fs::metadata(path) {
    Ok(metadata) => Ok(metadata.len()),
    Err(error) if StoreError::is_missing(&error) => Ok(0),
    Err(error) => Err(StoreError::Io(error)),
  }
}

fn dir_bytes(path: &Path) -> Result<u64, StoreError> {
  let mut total = 0u64;
  for entry in fs::read_dir(path)? {
    let entry = entry?;
    let file_type = entry.file_type()?;
    if file_type.is_dir() {
      total += dir_bytes(&entry.path())?;
    } else {
      total += entry.metadata()?.len();
    }
  }
  Ok(total)
}

fn remove_file_if_present(path: &Path) -> Result<(), StoreError> {
  match fs::remove_file(path) {
    Ok(()) => Ok(()),
    Err(error) if StoreError::is_missing(&error) => Ok(()),
    Err(error) => Err(StoreError::Io(error)),
  }
}

#[cfg(test)]
mod tests {
  use pi_rs_core::hash::sha256_hex;

  use crate::tmp::TempDir;

  use super::*;

  #[test]
  fn create_makes_a_private_root() {
    let root = TempDir::new("layout-create");
    let layout = StateLayout::new(root.path());
    layout.create().unwrap();
    assert!(layout.sessions_dir().is_dir());
    assert!(layout.artifacts_dir().is_dir());
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      let mode = fs::metadata(layout.root()).unwrap().permissions().mode() & 0o777;
      assert_eq!(mode, 0o700, "state directory must not be world-readable");
    }
    // Idempotent: open is called on every launch.
    layout.create().unwrap();
  }

  #[test]
  fn paths_are_contained_and_session_relative() {
    let layout = StateLayout::new("/state");
    let session = SessionId::from_string("018f-session".to_string());
    assert_eq!(
      layout.session_path(&session),
      PathBuf::from("/state/sessions/018f-session.jsonl")
    );
    assert_eq!(
      layout.trace_path(&session),
      PathBuf::from("/state/sessions/018f-session.trace.jsonl")
    );
    let blob = BlobRef::for_bytes(b"payload", Some("text/plain"));
    assert_eq!(
      layout.blob_path(&session, &blob),
      PathBuf::from(format!(
        "/state/sessions/018f-session/blobs/{}",
        &blob.hash[..2]
      ))
      .join(blob.hash.clone())
    );
    assert_eq!(layout.blob_relative_path(&blob), blob.relative_path());
    assert!(
      layout
        .blob_relative_path(&blob)
        .contains(&sha256_hex(b"payload")),
      "reference must name the content it points at"
    );
    assert_eq!(
      layout.checkpoint_path(&session, &CheckpointId::from_string("cp1".to_string())),
      PathBuf::from("/state/sessions/018f-session/checkpoints/cp1.json")
    );
  }

  #[test]
  fn listing_is_newest_first_without_parsing_bodies() {
    let root = TempDir::new("layout-list");
    let layout = StateLayout::new(root.path());
    layout.create().unwrap();
    for id in ["018f-aaa", "0190-bbb", "018e-ccc"] {
      let session = SessionId::from_string(id.to_string());
      layout.ensure_session_dirs(&session).unwrap();
      fs::write(layout.session_path(&session), "{}\n").unwrap();
      fs::write(layout.trace_path(&session), "{}\n").unwrap();
    }
    // Trace journals and names that are not legal identifiers are not sessions.
    // The distinction is identifier legality, not "looks like a note": a lowercase
    // name like `notes` is a legal session id, so a file by that name *is* a
    // session under this convention and is not silently ignored.
    fs::write(layout.sessions_dir().join("Notes.jsonl"), "x\n").unwrap();
    fs::write(layout.sessions_dir().join("index.json"), "x\n").unwrap();
    let ids = layout.list_session_ids().unwrap();
    let names: Vec<&str> = ids.iter().map(|id| id.as_str()).collect();
    assert_eq!(names, vec!["0190-bbb", "018f-aaa", "018e-ccc"]);
    let on_disk: u64 = fs::read_dir(layout.sessions_dir())
      .unwrap()
      .filter_map(Result::ok)
      .filter(|entry| entry.path().is_file())
      .map(|entry| entry.metadata().unwrap().len())
      .sum();
    assert_eq!(layout.state_bytes().unwrap(), on_disk);
  }

  #[test]
  fn missing_root_lists_no_sessions() {
    let layout = StateLayout::new("/state/does/not/exist");
    assert!(layout.list_session_ids().unwrap().is_empty());
    assert_eq!(layout.state_bytes().unwrap(), 0);
  }

  #[test]
  fn session_ids_cannot_escape_the_sessions_directory() {
    assert!(
      StateLayout::validate_session_id(&SessionId::from_string("018f-ok".to_string())).is_ok()
    );
    for bad in ["../evil", "a/b", "DototDot", "", &"x".repeat(65)] {
      let error = StateLayout::validate_session_id(&SessionId::from_string(bad.to_string()))
        .expect_err("must reject");
      assert!(matches!(error, Invalid(_)), "{bad}: {error}");
    }
  }

  #[test]
  fn removal_deletes_everything_belonging_to_one_session() {
    let root = TempDir::new("layout-remove");
    let layout = StateLayout::new(root.path());
    layout.create().unwrap();
    let session = SessionId::from_string("018f-gone".to_string());
    layout.ensure_session_dirs(&session).unwrap();
    fs::write(layout.session_path(&session), "{}\n").unwrap();
    fs::write(layout.trace_path(&session), "{}\n").unwrap();
    let blob = BlobRef::for_bytes(b"payload", None);
    let blob_path = layout.blob_path(&session, &blob);
    fs::create_dir_all(blob_path.parent().unwrap()).unwrap();
    fs::write(&blob_path, "payload").unwrap();
    assert!(layout.session_bytes(&session).unwrap() > 0);

    layout.remove_session(&session).unwrap();
    assert!(!layout.session_path(&session).exists());
    assert!(!layout.session_dir(&session).exists());
    assert_eq!(layout.session_bytes(&session).unwrap(), 0);
    // Removing twice is not an error: retention races with itself harmlessly.
    layout.remove_session(&session).unwrap();
  }
}
