//! Bounded trace retention.
//!
//! Defaults are bounded on purpose: an unbounded journal is a disk incident, and
//! trace data may contain secrets, so "keep everything forever" is not a
//! neutral choice. Two independent bounds exist because they answer different
//! concerns — age answers "how long can I still explain this session?", size
//! answers "how much may this cost me today?".
//!
//! Age comes from the session identifier, not from file mtimes. Session ids are
//! time-ordered UUIDv7 values, so age survives copies, restores, and
//! filesystems that do not preserve modification times. Identifiers that are not
//! UUIDv7-shaped (an imported session, for example) have no derivable age and
//! are therefore never age-deleted: deleting a session whose age is unknown
//! would be guessing with someone else's history.

use pi_rs_core::{ids::SessionId, trace::TraceRetention};

use crate::{StateLayout, StoreError};

/// Sessions never deleted by retention, regardless of the configured bounds.
///
/// In practice this is what protects the session the user is currently working
/// in; the store deliberately does not track open handles, because retention
/// must also work on a state directory no process owns.
pub const DEFAULT_KEEP_NEWEST: usize = 3;

const DAY_MS: u64 = 86_400_000;

/// What a retention pass deleted, or would delete.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetentionReport {
  /// Sessions removed, oldest first, with the bytes each held.
  pub removed: Vec<(SessionId, u64)>,
  pub freed_bytes: u64,
  /// Bytes still held under the sessions directory.
  pub remaining_bytes: u64,
  /// Sessions protected by `keep_newest`, so that "nothing was deleted" is
  /// explainable instead of mysterious.
  pub protected: usize,
  /// Sessions whose age could not be derived from the identifier.
  pub undated: usize,
}

impl RetentionReport {
  pub fn removed_count(&self) -> usize {
    self.removed.len()
  }

  pub fn is_noop(&self) -> bool {
    self.removed.is_empty()
  }
}

/// Delete expired and over-cap sessions.
///
/// `now_ms` is supplied rather than read from the clock so that a pass is
/// testable and idempotent.
pub fn apply(
  layout: &StateLayout,
  retention: &TraceRetention,
  now_ms: u64,
  keep_newest: usize,
) -> Result<RetentionReport, StoreError> {
  let plan = plan(layout, retention, now_ms, keep_newest)?;
  for (id, _) in &plan.victims {
    layout.remove_session(id)?;
  }
  Ok(plan.report)
}

/// Report what a pass would delete, deleting nothing.
///
/// Retention destroys data that cannot be recovered afterwards, so the plan is
/// a first-class operation rather than a log line the user has to trust.
pub fn dry_run(
  layout: &StateLayout,
  retention: &TraceRetention,
  now_ms: u64,
  keep_newest: usize,
) -> Result<RetentionReport, StoreError> {
  Ok(plan(layout, retention, now_ms, keep_newest)?.report)
}

#[derive(Debug)]
struct Plan {
  report: RetentionReport,
  victims: Vec<(SessionId, u64)>,
}

/// Shared planning step: one order, one set of victims, used by both paths so
/// that a dry run can never drift from a real pass.
fn plan(
  layout: &StateLayout,
  retention: &TraceRetention,
  now_ms: u64,
  keep_newest: usize,
) -> Result<Plan, StoreError> {
  let ids = layout.list_session_ids()?;
  let mut report = RetentionReport {
    protected: ids.len().min(keep_newest),
    remaining_bytes: layout.state_bytes()?,
    ..RetentionReport::default()
  };
  let mut victims = Vec::new();
  if retention.max_age_days.is_none() && retention.max_bytes.is_none() {
    return Ok(Plan { report, victims });
  }

  let mut candidates: Vec<(u64, SessionId, u64)> = Vec::new();
  for (index, id) in ids.iter().enumerate() {
    if index < keep_newest {
      continue;
    }
    let bytes = layout.session_bytes(id)?;
    match session_started_ms(id) {
      Some(started) => candidates.push((started, id.clone(), bytes)),
      None => report.undated += 1,
    }
  }

  // Oldest first: retention exists to shed history, never the recent past, so
  // deletion order must not depend on directory iteration order.
  candidates.sort_by_key(|(started, _, _)| *started);

  let age_limit = retention
    .max_age_days
    .map(|days| now_ms.saturating_sub(days.saturating_mul(DAY_MS)));
  let cap = retention.max_bytes;
  let mut remaining = report.remaining_bytes;
  let mut oversized = cap.is_some_and(|limit| remaining > limit);

  for (started, id, bytes) in candidates {
    let expired = age_limit.is_some_and(|limit| started < limit);
    if !expired && !oversized {
      break;
    }
    victims.push((id, bytes));
    remaining = remaining.saturating_sub(bytes);
    oversized = cap.is_some_and(|limit| remaining > limit);
  }

  // Exact arithmetic: the total minus what was removed. A dry run reports the
  // same prediction it planned with, which is why the two paths can never
  // disagree about how much they freed.
  report.remaining_bytes = remaining;
  report.freed_bytes = victims.iter().map(|(_, bytes)| bytes).sum();
  report.removed = victims.clone();
  Ok(Plan { report, victims })
}

/// Millisecond timestamp encoded in a UUIDv7-shaped session id.
pub fn session_started_ms(id: &SessionId) -> Option<u64> {
  let hex: String = id
    .as_str()
    .chars()
    .filter(|ch| *ch != '-')
    .take(12)
    .collect();
  if hex.len() != 12 || !hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
    return None;
  }
  let value = u64::from_str_radix(&hex, 16).ok()?;
  // A plausible wall-clock millisecond is at least a 40-bit magnitude. Smaller
  // values come from hand-written identifiers, not from minted ids.
  if value < 1_000_000_000_000 {
    return None;
  }
  Some(value)
}

#[cfg(test)]
mod tests {
  use std::fs;

  use pi_rs_core::{ids::uuidv7, trace::RawPayloadCapture};

  use crate::tmp::TempDir;

  use super::*;

  fn opened(tmp: &TempDir) -> StateLayout {
    let layout = StateLayout::new(tmp.path());
    layout.create().unwrap();
    layout
  }

  /// A session id with the UUIDv7 shape and timestamp prefix of `started_ms`.
  /// Deterministic, so that retention tests can name the survivors.
  fn id_from_millis(millis: u64) -> SessionId {
    SessionId::from_string(format!("{:012x}-7000-8000-000000000000", millis))
  }

  fn written(layout: &StateLayout, started_ms: u64, bytes: usize) -> SessionId {
    let id = id_from_millis(started_ms);
    layout.ensure_session_dirs(&id).unwrap();
    fs::write(layout.trace_path(&id), "x".repeat(bytes)).unwrap();
    fs::write(
      layout.session_path(&id),
      format!(
        "{{\"type\":\"header\",\"session_id\":\"{id}\",\"version\":1,\"started_at_ms\":{started_ms},\"working_dir\":\"/r\",\"model\":\"local/m\"}}\n"
      ),
    )
    .unwrap();
    id
  }

  /// Bytes of one session's header line. Every id in these tests has the same
  /// length, so the header size is a constant worth measuring instead of
  /// assuming.
  fn header_len() -> u64 {
    let tmp = TempDir::new("retention-header");
    let probe = opened(&tmp);
    let id = written(&probe, 1_700_000_000_000, 0);
    fs::metadata(probe.session_path(&id)).unwrap().len()
  }

  fn retention(age: Option<u64>, bytes: Option<u64>) -> TraceRetention {
    TraceRetention {
      max_age_days: age,
      max_bytes: bytes,
      ..TraceRetention::default()
    }
  }

  #[test]
  fn age_is_read_from_the_identifier_not_the_filesystem() {
    assert_eq!(
      session_started_ms(&id_from_millis(1_700_000_000_000)),
      Some(1_700_000_000_000)
    );
    let minted = SessionId::from_string(uuidv7());
    let started = session_started_ms(&minted).expect("minted ids are dated");
    assert!(
      (1_700_000_000_000..4_000_000_000_000).contains(&started),
      "{minted} -> {started}"
    );
    assert_eq!(
      session_started_ms(&SessionId::from_string("legacy-session")),
      None
    );
    assert_eq!(
      session_started_ms(&SessionId::from_string(
        "00000000-0000-7000-8000-000000000000"
      )),
      None,
      "hand-written identifiers are not treated as dated"
    );
  }

  #[test]
  fn nothing_is_deleted_without_a_configured_bound() {
    let tmp = TempDir::new("retention-off");
    let layout = opened(&tmp);
    let header = header_len();
    for days in 0..10u64 {
      written(&layout, 1_700_000_000_000 - days * DAY_MS, 1_000);
    }
    let report = apply(
      &layout,
      &retention(None, None),
      1_700_000_000_000,
      DEFAULT_KEEP_NEWEST,
    )
    .unwrap();
    assert!(report.is_noop());
    assert_eq!(report.remaining_bytes, 10 * (1_000 + header));
    assert_eq!(layout.list_session_ids().unwrap().len(), 10);
  }

  #[test]
  fn expired_sessions_go_and_recent_ones_stay() {
    let tmp = TempDir::new("retention-age");
    let layout = opened(&tmp);
    let header = header_len();
    let now = 1_800_000_000_000u64;
    let ids: Vec<SessionId> = [1u64, 10, 30, 90, 200]
      .iter()
      .map(|days| written(&layout, now - days * DAY_MS, 2_000))
      .collect();
    let report = apply(
      &layout,
      &retention(Some(45), None),
      now,
      DEFAULT_KEEP_NEWEST,
    )
    .unwrap();
    assert_eq!(
      report.removed_count(),
      2,
      "90 and 200 days are both past 45: {report:?}"
    );
    assert!(!layout.session_path(&ids[4]).exists());
    assert!(!layout.session_path(&ids[3]).exists());
    assert_eq!(layout.list_session_ids().unwrap().len(), 3);
    assert_eq!(report.freed_bytes, 2 * (2_000 + header));
  }

  #[test]
  fn byte_pressure_drops_oldest_first_and_protects_the_recent_ones() {
    let tmp = TempDir::new("retention-bytes");
    let layout = opened(&tmp);
    let now = 1_800_000_000_000u64;
    for days in [0u64, 1, 2, 3, 4, 5] {
      written(&layout, now - days * DAY_MS, 4_000);
    }
    let report = apply(
      &layout,
      &retention(None, Some(20_000)),
      now,
      DEFAULT_KEEP_NEWEST,
    )
    .unwrap();
    assert!(
      report.remaining_bytes <= 20_000,
      "cap must be met: {report:?}"
    );
    assert_eq!(report.protected, DEFAULT_KEEP_NEWEST);
    let survivors = layout.list_session_ids().unwrap();
    assert_eq!(
      survivors,
      vec![
        id_from_millis(now),
        id_from_millis(now - DAY_MS),
        id_from_millis(now - 2 * DAY_MS),
        id_from_millis(now - 3 * DAY_MS),
      ],
      "the protected newest survive, plus the one session that fit under the cap"
    );
  }

  #[test]
  fn protection_can_be_disabled_for_a_full_sweep() {
    let tmp = TempDir::new("retention-sweep");
    let layout = opened(&tmp);
    let now = 1_800_000_000_000u64;
    for days in [0u64, 100, 400] {
      written(&layout, now - days * DAY_MS, 1_000);
    }
    let report = apply(&layout, &retention(Some(30), None), now, 0).unwrap();
    assert_eq!(report.removed_count(), 2, "{report:?}");
    assert_eq!(layout.list_session_ids().unwrap().len(), 1);
  }

  #[test]
  fn undated_sessions_are_never_age_deleted() {
    let tmp = TempDir::new("retention-undated");
    let layout = opened(&tmp);
    let now = 1_800_000_000_000u64;
    let fresh = written(&layout, now, 1_000);
    let legacy = {
      let id = SessionId::from_string("legacy-session");
      layout.ensure_session_dirs(&id).unwrap();
      fs::write(layout.trace_path(&id), "x".repeat(5_000)).unwrap();
      fs::write(
        layout.session_path(&id),
        format!(
          "{{\"type\":\"header\",\"session_id\":\"{id}\",\"version\":1,\"started_at_ms\":0,\"working_dir\":\"/r\",\"model\":\"local/m\"}}\n"
        ),
      )
      .unwrap();
      id
    };
    let report = apply(&layout, &retention(Some(1), None), now, 0).unwrap();
    assert_eq!(report.undated, 1);
    assert!(report.is_noop(), "{report:?}");
    assert_eq!(
      layout.list_session_ids().unwrap().first().unwrap(),
      &legacy,
      "undated sorts newest-first by name only"
    );
    assert!(layout.session_path(&legacy).exists());
    assert!(layout.session_path(&fresh).exists());
  }

  #[test]
  fn dry_run_matches_the_real_pass_and_deletes_nothing() {
    let tmp = TempDir::new("retention-dry");
    let layout = opened(&tmp);
    let now = 1_800_000_000_000u64;
    for days in [0u64, 1, 2, 3, 4, 5] {
      written(&layout, now - days * DAY_MS, 4_000);
    }
    let before = layout.state_bytes().unwrap();
    let planned = dry_run(
      &layout,
      &retention(None, Some(20_000)),
      now,
      DEFAULT_KEEP_NEWEST,
    )
    .unwrap();
    assert!(planned.removed_count() > 0, "the cap is exceeded");
    assert_eq!(
      layout.state_bytes().unwrap(),
      before,
      "a dry run deletes nothing"
    );
    let real = apply(
      &layout,
      &retention(None, Some(20_000)),
      now,
      DEFAULT_KEEP_NEWEST,
    )
    .unwrap();
    assert_eq!(real.freed_bytes, planned.freed_bytes);
    assert_eq!(real.remaining_bytes, planned.remaining_bytes);
    assert_eq!(real.removed, planned.removed);
  }

  #[test]
  fn raw_capture_is_off_by_so_retention_has_nothing_extra_to_hide() {
    assert_eq!(
      TraceRetention::default().raw_payload,
      RawPayloadCapture::Disabled
    );
  }
}
