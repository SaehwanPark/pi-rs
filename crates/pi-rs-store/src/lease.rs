//! Exclusive ownership leases for open sessions.
//!
//! A session is a linear append-only conversation. The lease directory contains
//! a kernel-backed advisory lock file, so the operating system releases
//! ownership when the process exits—even after SIGKILL, OOM termination, or a
//! machine reboot. The marker beside it is diagnostic only; it is never used as
//! the mutual-exclusion primitive.
//!
//! Lease directories from the pre-lock format are accepted conservatively. They
//! are consulted only when the new lock file is absent, and their PID marker is
//! never used for leases written by this build; an old marker may require manual
//! cleanup when no kernel lock file exists.

use std::{
  fs::{self, File, OpenOptions},
  io::{self, Read, Seek, SeekFrom, Write},
  path::Path,
};

use fs4::FileExt;
use pi_rs_core::ids::uuidv7;

use crate::StoreError;

const OWNER_FILE: &str = "owner";
const LOCK_FILE: &str = "lock";
const LOCK_MARKER: &str = "pi-rs-lock-v1";

/// One held session lease. Dropping the file releases the kernel lock. The
/// directory and marker remain as a cheap diagnostic record and so that a
/// future opener never has to race a remove/recreate operation.
#[derive(Debug)]
pub(crate) struct SessionLease {
  file: File,
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
    match fs::create_dir(path) {
      Ok(()) => {}
      Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
      Err(error) => return Err(StoreError::Io(error)),
    }

    let lock_path = path.join(LOCK_FILE);
    let lock_preexists = lock_path.exists();
    // A directory written by an older pi-rs process has an owner marker but no
    // lock file. Do not let a new process overlap a still-running old writer;
    // once a lock file exists, even an empty one, the kernel lock is the only
    // authority. This branch is one-time migration compatibility, not normal
    // lease liveness.
    if !lock_preexists && path.join(OWNER_FILE).exists() && legacy_owner_alive(path) {
      return Err(active_error(path));
    }
    let mut file = match OpenOptions::new()
      .read(true)
      .write(true)
      .create_new(true)
      .open(&lock_path)
    {
      Ok(file) => {
        sync_lease_directory(path)?;
        file
      }
      Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
        OpenOptions::new().read(true).write(true).open(&lock_path)?
      }
      Err(error) => return Err(StoreError::Io(error)),
    };

    if let Err(error) = file.try_lock_exclusive() {
      if is_lock_contended(&error) {
        return Err(active_error(path));
      }
      return Err(StoreError::Io(error));
    }

    if let Err(error) = ensure_lock_marker(&mut file) {
      let _ = fs4::FileExt::unlock(&file);
      return Err(error);
    }
    let token = uuidv7();
    if let Err(error) = write_owner_marker(path, &token) {
      let _ = fs4::FileExt::unlock(&file);
      return Err(StoreError::Io(error));
    }

    Ok(Self { file })
  }

  /// Whether a lease is held by another process. This attempts the same
  /// nonblocking kernel lock used by [`Self::acquire`], so stale markers and
  /// dead PIDs cannot make a current lease permanent.
  pub fn is_active(path: &Path) -> bool {
    if !path.exists() {
      return false;
    }
    let lock_path = path.join(LOCK_FILE);
    if !lock_path.exists() {
      // An old-format directory may not have a lock file yet. Be conservative
      // when the owner marker is unreadable; retention must not delete a live
      // session merely because it cannot inspect its compatibility marker.
      return path.join(OWNER_FILE).exists() && legacy_owner_alive(path);
    }
    let Ok(file) = OpenOptions::new().read(true).write(true).open(lock_path) else {
      return true;
    };
    match file.try_lock_exclusive() {
      Ok(()) => {
        let _ = fs4::FileExt::unlock(&file);
        false
      }
      Err(error) if is_lock_contended(&error) => true,
      Err(_) => true,
    }
  }
}

impl Drop for SessionLease {
  fn drop(&mut self) {
    // Unlock explicitly for clarity; dropping the handle would release it too.
    // The path is intentionally retained: deleting a lock file after unlock can
    // race a new opener and remove its active lock from under it.
    let _ = fs4::FileExt::unlock(&self.file);
  }
}

fn active_error(path: &Path) -> StoreError {
  StoreError::Invalid(format!("session is active elsewhere ({})", path.display()))
}

fn is_lock_contended(error: &io::Error) -> bool {
  error.kind() == io::ErrorKind::WouldBlock
    || error
      .raw_os_error()
      .zip(fs4::lock_contended_error().raw_os_error())
      .is_some_and(|(actual, contended)| actual == contended)
}

fn ensure_lock_marker(file: &mut File) -> Result<(), StoreError> {
  file.seek(SeekFrom::Start(0))?;
  let mut contents = Vec::new();
  file.read_to_end(&mut contents)?;
  if contents.is_empty() {
    // The file's existence is the durable format marker. An empty file can only
    // be a newly created marker whose publication was interrupted; fill it while
    // holding the kernel lock, without a destructive truncate/rewrite.
    file.seek(SeekFrom::Start(0))?;
    file.write_all(format!("{LOCK_MARKER}\n").as_bytes())?;
    file.flush()?;
    file.sync_all()?;
    sync_lease_directory_from_file(file)?;
    return Ok(());
  }
  if contents
    .split(|byte| *byte == b'\n')
    .next()
    .is_some_and(|line| line == LOCK_MARKER.as_bytes())
  {
    return Ok(());
  }
  Err(StoreError::Invalid(
    "session lease lock marker is corrupt; manual recovery required".into(),
  ))
}

#[cfg(unix)]
fn sync_lease_directory(path: &Path) -> Result<(), StoreError> {
  File::open(path)?.sync_all()?;
  Ok(())
}

#[cfg(not(unix))]
fn sync_lease_directory(_path: &Path) -> Result<(), StoreError> {
  Ok(())
}

#[cfg(unix)]
fn sync_lease_directory_from_file(file: &File) -> Result<(), StoreError> {
  // The parent directory is not recoverable from a handle alone on every
  // platform; the file sync above is the required marker durability boundary.
  let _ = file;
  Ok(())
}

#[cfg(not(unix))]
fn sync_lease_directory_from_file(_file: &File) -> Result<(), StoreError> {
  Ok(())
}

fn write_owner_marker(path: &Path, token: &str) -> io::Result<()> {
  let mut file = OpenOptions::new()
    .write(true)
    .create(true)
    .truncate(true)
    .open(path.join(OWNER_FILE))?;
  // The lock is held while publishing the diagnostic marker, so a concurrent
  // reader can never mistake this compatibility file for ownership authority.
  writeln!(file, "{}\n{token}", std::process::id())?;
  file.sync_all()
}

/// Check a pre-lock lease marker only while migrating an old directory. New
/// leases use the kernel lock and never consult a PID.
fn legacy_owner_alive(path: &Path) -> bool {
  let Ok(contents) = fs::read_to_string(path.join(OWNER_FILE)) else {
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
  legacy_process_alive(pid)
}

#[cfg(target_os = "linux")]
fn legacy_process_alive(pid: u32) -> bool {
  // Linux exposes process identity as a filesystem entry. A reused PID is
  // conservatively treated as active, preserving safety for old leases.
  fs::metadata(format!("/proc/{pid}")).is_ok()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn legacy_process_alive(pid: u32) -> bool {
  // This path is used only for a pre-lock lease left by an older release. The
  // kernel lock remains the authority for all leases written by this build.
  let Some(pid) = rustix::process::Pid::from_raw(pid as rustix::process::RawPid) else {
    return true;
  };
  match rustix::process::test_kill_process(pid) {
    Ok(()) => true,
    Err(error) => error.raw_os_error() == rustix::io::Errno::PERM.raw_os_error(),
  }
}

#[cfg(windows)]
fn legacy_process_alive(_pid: u32) -> bool {
  // The pre-lock format has no cross-platform ownership primitive. Keep an old
  // marker conservative rather than spawning `tasklist` or introducing unsafe
  // process-table bindings; all leases written by this build use the kernel
  // lock and are released automatically on process exit.
  true
}

#[cfg(not(any(unix, windows)))]
fn legacy_process_alive(_pid: u32) -> bool {
  true
}

#[cfg(test)]
mod tests {
  use std::{
    path::PathBuf,
    process::Command,
    thread,
    time::{Duration, Instant},
  };

  use crate::tmp::TempDir;

  use super::*;

  #[test]
  fn kernel_lock_blocks_a_second_holder_and_releases_on_drop() {
    let tmp = TempDir::new("lease-lock");
    let path = tmp.path().join("session.lease");
    let first = SessionLease::acquire(&path).unwrap();
    assert!(SessionLease::is_active(&path));
    assert!(matches!(
      SessionLease::acquire(&path),
      Err(StoreError::Invalid(message)) if message.contains("active elsewhere")
    ));
    drop(first);
    assert!(!SessionLease::is_active(&path));
    let second = SessionLease::acquire(&path).unwrap();
    drop(second);
  }

  #[test]
  fn an_existing_empty_lock_file_is_not_reinterpreted_as_a_legacy_lease() {
    let tmp = TempDir::new("lease-empty-marker");
    let path = tmp.path().join("session.lease");
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join(LOCK_FILE), b"").unwrap();
    fs::write(
      path.join(OWNER_FILE),
      format!("{}\nold-token", std::process::id()),
    )
    .unwrap();

    let lease = SessionLease::acquire(&path).expect("kernel lock owns an existing marker file");
    drop(lease);
    let marker = fs::read_to_string(path.join(LOCK_FILE)).unwrap();
    assert_eq!(marker, format!("{LOCK_MARKER}\n"));
  }

  #[test]
  fn lock_is_released_when_owner_process_is_killed() {
    let child_path = std::env::var_os("PI_RS_LEASE_CHILD");
    if let Some(path) = child_path {
      let path = PathBuf::from(path);
      let _lease = SessionLease::acquire(&path).expect("child acquires lease");
      fs::write(path.with_extension("ready"), b"ready").expect("child publishes readiness");
      loop {
        thread::sleep(Duration::from_secs(1));
      }
    }

    let tmp = TempDir::new("lease-kill");
    let path = tmp.path().join("session.lease");
    let ready = path.with_extension("ready");
    let mut child = Command::new(std::env::current_exe().expect("test executable"))
      .args([
        "--exact",
        "lease::tests::lock_is_released_when_owner_process_is_killed",
        "--nocapture",
      ])
      .env("PI_RS_LEASE_CHILD", &path)
      .spawn()
      .expect("spawn lease child");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() && Instant::now() < deadline {
      if child.try_wait().expect("poll child").is_some() {
        break;
      }
      thread::sleep(Duration::from_millis(20));
    }
    assert!(ready.exists(), "child did not acquire its lease");
    assert!(SessionLease::is_active(&path));
    child.kill().expect("kill lease child");
    let _ = child.wait();
    assert!(!SessionLease::is_active(&path));
    let _lease = SessionLease::acquire(&path).expect("dead owner lock is reclaimable");
  }
}
