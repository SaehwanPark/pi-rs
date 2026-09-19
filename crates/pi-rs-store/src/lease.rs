//! Exclusive ownership leases for open sessions.
//!
//! A session is a linear append-only conversation. The lease is represented by
//! an atomically-created directory so a second process cannot reopen the same
//! journals and allocate duplicate sequence numbers. The owner marker lets a
//! later process distinguish a live lease from one left by a crashed process;
//! uncertainty is treated as active rather than risking concurrent writers.

use std::{
  fs,
  path::{Path, PathBuf},
};

use pi_rs_core::ids::uuidv7;

use crate::StoreError;

const OWNER_FILE: &str = "owner";

/// One held session lease. Dropping it releases ownership only when the marker
/// still names this exact holder.
#[derive(Debug)]
pub(crate) struct SessionLease {
  path: PathBuf,
  token: String,
}

impl SessionLease {
  pub fn acquire(path: &Path) -> Result<Self, StoreError> {
    let parent = path.parent().ok_or_else(|| {
      StoreError::Invalid(format!(
        "session lease path {} has no parent",
        path.display()
      ))
    })?;
    fs::create_dir_all(parent)?;
    for _ in 0..3 {
      match fs::create_dir(path) {
        Ok(()) => {
          let token = uuidv7();
          let owner = path.join(OWNER_FILE);
          let temporary = path.join("owner.tmp");
          // The directory itself is the lock. Write the marker through a
          // temporary path so readers never mistake a partial owner for a
          // trustworthy liveness answer.
          fs::write(&temporary, format!("{}\n{token}\n", std::process::id()))?;
          fs::rename(&temporary, &owner)?;
          return Ok(Self {
            path: path.to_path_buf(),
            token,
          });
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
          if owner_alive(path) {
            return Err(StoreError::Invalid(format!(
              "session is active elsewhere ({})",
              path.display()
            )));
          }
          // A dead owner's directory is safe to reclaim. If another process
          // wins the race to replace it, the next iteration observes its live
          // marker and refuses.
          // A lease directory contains only its owner marker. Remove the
          // directory itself, not recursively: if another process won the
          // race and recreated a live lease, `NotEmpty` preserves that lock
          // instead of deleting an active writer out from under it.
          match fs::remove_dir(path) {
            Ok(()) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(StoreError::Io(error)),
          }
        }
        Err(error) => return Err(StoreError::Io(error)),
      }
    }
    Err(StoreError::Invalid(format!(
      "session lease {} changed while recovering a stale owner",
      path.display()
    )))
  }

  /// Whether a lease is held by a live process. A missing or unreadable marker
  /// is conservatively considered active.
  pub fn is_active(path: &Path) -> bool {
    path.exists() && owner_alive(path)
  }
}

impl Drop for SessionLease {
  fn drop(&mut self) {
    let owner = self.path.join(OWNER_FILE);
    let Ok(contents) = fs::read_to_string(owner) else {
      return;
    };
    let mut lines = contents.lines();
    let _pid = lines.next();
    if lines.next() != Some(self.token.as_str()) {
      return;
    }
    let _ = fs::remove_dir_all(&self.path);
  }
}

fn owner_alive(path: &Path) -> bool {
  let Ok(contents) = fs::read_to_string(path.join(OWNER_FILE)) else {
    // The lock directory may be between mkdir and the atomic owner rename.
    return true;
  };
  let Some(pid) = contents
    .lines()
    .next()
    .and_then(|line| line.parse::<u32>().ok())
  else {
    return true;
  };
  if pid == std::process::id() {
    return true;
  }
  process_alive(pid)
}

#[cfg(target_os = "linux")]
fn process_alive(pid: u32) -> bool {
  // Linux exposes process identity as a filesystem entry. Unlike `kill -0`,
  // this does not spawn a helper for every retention candidate. A reused PID
  // is conservatively treated as active, preserving the no-concurrent-writer
  // invariant at the cost of leaving one stale lease for later cleanup.
  fs::metadata(format!("/proc/{pid}")).is_ok()
}

// Platforms without a cheap, non-spawning process table are conservative:
// an owner is treated as active rather than launching `kill`, `tasklist`, or a
// similar helper once per retention candidate. This may leave stale leases for
// a later platform-aware cleanup, but never risks deleting a live session and
// never makes retention process creation scale with the number of sessions.
#[cfg(not(target_os = "linux"))]
fn process_alive(_pid: u32) -> bool {
  true
}
