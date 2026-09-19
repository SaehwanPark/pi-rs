//! Content-addressed payload store.
//!
//! Payloads are addressed by SHA-256 of their bytes, which makes deduplication
//! automatic, makes a reference verifiable, and lets a reduced event keep a
//! pointer that still resolves. The store writes through a temporary file and
//! renames, because a content-addressed store must never leave a half-written
//! object at the final path: the reference is derived from content, so a torn
//! object would be a permanent, silent lie.

use std::{
  fs::{self, OpenOptions},
  io::{Read, Write},
  path::{Path, PathBuf},
};

use flate2::{Compression as FlateCompression, read::DeflateDecoder, write::DeflateEncoder};
use pi_rs_core::{
  BlobCompression,
  hash::sha256_hex,
  ids::{SessionId, uuidv7},
  trace::BlobRef,
};

use crate::{StateLayout, StoreError};

/// Payload store rooted at one session's `blobs/` directory.
#[derive(Debug, Clone)]
pub struct BlobStore {
  base: PathBuf,
  compression: BlobCompression,
}

impl BlobStore {
  /// Prepare the payload directory for one session with compression disabled.
  pub fn for_session(layout: &StateLayout, session: &SessionId) -> Result<Self, StoreError> {
    Self::for_session_with_compression(layout, session, BlobCompression::None)
  }

  /// Prepare the payload directory for one session with an encoding preference.
  pub fn for_session_with_compression(
    layout: &StateLayout,
    session: &SessionId,
    compression: BlobCompression,
  ) -> Result<Self, StoreError> {
    StateLayout::validate_session_id(session)?;
    let base = layout.blobs_dir(session);
    fs::create_dir_all(&base)?;
    Ok(Self { base, compression })
  }

  /// Root directory of this store.
  pub fn base(&self) -> &Path {
    &self.base
  }

  /// Encoding preference used for newly written payloads.
  pub fn compression(&self) -> BlobCompression {
    self.compression
  }

  pub fn path_for(&self, blob: &BlobRef) -> PathBuf {
    self.base.join(&blob.relative_path()["blobs/".len()..])
  }

  /// Plan the reference that will be used for these logical bytes.
  ///
  /// Compression is a preference rather than a promise: a payload that does
  /// not shrink keeps the old raw path and reference shape.
  pub fn reference_for(
    &self,
    bytes: &[u8],
    content_type: Option<&str>,
  ) -> Result<BlobRef, StoreError> {
    let raw = BlobRef::for_bytes(bytes, content_type);
    if self.compression == BlobCompression::Deflate && deflate(bytes)?.len() < bytes.len() {
      Ok(raw.with_compression(BlobCompression::Deflate))
    } else {
      Ok(raw)
    }
  }

  /// Store bytes and return the reference that must be recorded durably.
  ///
  /// Existing matching content is not rewritten. Existing *disagreeing*
  /// content is rewritten rather than trusted: a reference is a claim about
  /// logical bytes, so a stored object that fails decoding, hashing, or sizing
  /// is repaired on the next write instead of silently persisting the lie.
  pub fn put(&self, bytes: &[u8], content_type: Option<&str>) -> Result<BlobRef, StoreError> {
    let blob = self.reference_for(bytes, content_type)?;
    let encoded = encode(bytes, blob.compression)?;
    let final_path = self.path_for(&blob);
    if final_path.exists() && self.verify(&blob)? {
      // Even a pre-existing valid object must have its directory entry synced
      // before a caller can durably publish a reference to it. This covers a
      // concurrent writer whose rename completed but whose directory sync has
      // not yet happened, as well as objects left by an older writer.
      sync_parent(&final_path)?;
      return Ok(blob);
    }
    if let Some(parent) = final_path.parent() {
      fs::create_dir_all(parent)?;
    }
    // The suffix is write-scoped, and create_new makes a collision a retryable
    // local error rather than allowing concurrent writers in one process to
    // truncate each other's temporary file before the rename.
    let temp = final_path.with_extension(format!("part-{}", uuidv7()));
    {
      let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
      file.write_all(&encoded)?;
      file.flush()?;
      // Returning a reference before this sync would let a durable journal line
      // outlive the blob bytes after a power loss.
      file.sync_all()?;
    }
    verify_file(&temp, &blob)?;
    match fs::rename(&temp, &final_path) {
      Ok(()) => {
        // The rename itself is atomic, but its directory entry is not durable
        // until the containing directory is synced where the platform supports
        // directory fsync.
        sync_parent(&final_path)?;
        Ok(blob)
      }
      Err(error) => {
        // Another writer may have won the race. Never accept that path merely
        // because it exists: verify the winner's decoded bytes before returning
        // a reference that may immediately become durable elsewhere.
        if final_path.exists() {
          if self.verify(&blob)? {
            sync_parent(&final_path)?;
            let _ = fs::remove_file(&temp);
            return Ok(blob);
          }
          // Windows does not replace an existing destination on rename. A
          // disagreeing object is safe to replace here because this writer's
          // content-addressed bytes are the same reference being repaired; if
          // another writer published the valid winner between the check and
          // removal, the final verification below still gates success.
          if fs::remove_file(&final_path).is_ok() {
            match fs::rename(&temp, &final_path) {
              Ok(()) if self.verify(&blob)? => {
                sync_parent(&final_path)?;
                return Ok(blob);
              }
              Ok(()) => {}
              Err(rename_error) => {
                let _ = fs::remove_file(&temp);
                if final_path.exists() && self.verify(&blob)? {
                  sync_parent(&final_path)?;
                  return Ok(blob);
                }
                return Err(StoreError::Io(rename_error));
              }
            }
          }
        }
        let _ = fs::remove_file(&temp);
        Err(StoreError::Io(error))
      }
    }
  }

  /// Store a JSON value with a canonical content type.
  pub fn put_json(&self, value: &serde_json::Value) -> Result<BlobRef, StoreError> {
    self.put(
      serde_json::to_vec(value)?.as_slice(),
      Some("application/json"),
    )
  }

  pub fn exists(&self, blob: &BlobRef) -> bool {
    self.path_for(blob).exists()
  }

  /// Remove one content-addressed payload when a transaction proves it is no
  /// longer referenced. Callers must perform the liveness check first.
  pub fn remove(&self, blob: &BlobRef) -> Result<(), StoreError> {
    let path = self.path_for(blob);
    match fs::remove_file(&path) {
      Ok(()) => sync_parent(&path),
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
      Err(error) => Err(StoreError::Io(error)),
    }
  }

  /// Read a payload, failing rather than returning empty when it is absent.
  /// Read the logical bytes a durable reference points at.
  ///
  /// A reference is what a bounded line records, and it arrives here as text from
  /// a file, so it is validated rather than trusted: it must be exactly the
  /// `blobs/<shard>/<hash>[.encoding]` shape the layout produces, with the shard
  /// equal to the hash's own prefix. Anything else is refused instead of joined,
  /// because a pointer read from a journal must not become a path outside this store.
  pub fn get_relative(&self, reference: &str) -> Result<Vec<u8>, StoreError> {
    let (path, compression) = self.parse_relative(reference)?;
    match fs::read(&path) {
      Ok(bytes) => decode(&bytes, compression),
      Err(error) if StoreError::is_missing(&error) => {
        Err(StoreError::Missing(reference.to_string()))
      }
      Err(error) => Err(StoreError::Io(error)),
    }
  }

  /// Read and verify a content-addressed payload referenced by a durable line.
  ///
  /// `get_relative` validates the path shape, while recovery also needs to
  /// reject a blob whose contents were replaced in place. The hash and logical
  /// size are part of the reference, so checking them here keeps a corrupted
  /// trace from being turned into a plausible message.
  pub fn get_relative_verified(&self, reference: &str) -> Result<Vec<u8>, StoreError> {
    let bytes = self.get_relative(reference)?;
    let filename = reference
      .rsplit('/')
      .next()
      .ok_or_else(|| StoreError::Invalid(format!("not a blob reference: {reference}")))?;
    let hash = filename.strip_suffix(".deflate").unwrap_or(filename);
    if hash.len() != 64 || sha256_hex(&bytes) != hash {
      return Err(StoreError::Invalid(format!(
        "blob reference does not match its contents: {reference}"
      )));
    }
    Ok(bytes)
  }

  pub fn get(&self, blob: &BlobRef) -> Result<Vec<u8>, StoreError> {
    let path = self.path_for(blob);
    match fs::read(&path) {
      Ok(bytes) => decode(&bytes, blob.compression),
      Err(error) if StoreError::is_missing(&error) => {
        Err(StoreError::Missing(path.display().to_string()))
      }
      Err(error) => Err(StoreError::Io(error)),
    }
  }

  /// Read a payload as UTF-8 text without losing the distinction between
  /// "absent" and "not text".
  pub fn get_text(&self, blob: &BlobRef) -> Result<String, StoreError> {
    let bytes = self.get(blob)?;
    String::from_utf8(bytes).map_err(|_| {
      StoreError::Invalid(format!(
        "blob {} is not valid UTF-8; use get() for binary payloads",
        blob.short_hash()
      ))
    })
  }

  /// Verify that stored logical bytes still match their reference.
  pub fn verify(&self, blob: &BlobRef) -> Result<bool, StoreError> {
    match fs::read(self.path_for(blob)) {
      Ok(bytes) => match decode(&bytes, blob.compression) {
        Ok(bytes) => Ok(sha256_hex(&bytes) == blob.hash && bytes.len() as u64 == blob.size),
        Err(StoreError::Invalid(_)) => Ok(false),
        Err(error) => Err(error),
      },
      Err(error) if StoreError::is_missing(&error) => Ok(false),
      Err(error) => Err(StoreError::Io(error)),
    }
  }

  fn parse_relative(&self, reference: &str) -> Result<(PathBuf, BlobCompression), StoreError> {
    let invalid = || StoreError::Invalid(format!("not a blob reference: {reference}"));
    let rest = reference.strip_prefix("blobs/").ok_or_else(invalid)?;
    let (shard, filename) = rest.split_once('/').ok_or_else(invalid)?;
    if filename.contains('/') || reference.contains("..") {
      return Err(invalid());
    }
    let (hash, suffix) = filename
      .rsplit_once('.')
      .map_or((filename, None), |(hash, suffix)| (hash, Some(suffix)));
    let compression = BlobCompression::from_suffix(suffix).ok_or_else(invalid)?;
    if hash.len() != 64
      || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
      || shard.len() != 2
      || !hash.starts_with(shard)
    {
      return Err(invalid());
    }
    Ok((self.base.join(shard).join(filename), compression))
  }

  /// Delete payloads that no listed record references.
  ///
  /// Referenced paths are supplied by the caller rather than discovered here:
  /// liveness has to be decided from durable records, and a scan for "unused"
  /// files must never decide on its own that a payload nobody named in the
  /// window it looked at is garbage.
  pub fn prune_unreferenced(
    &self,
    referenced: &std::collections::BTreeSet<String>,
  ) -> Result<u64, StoreError> {
    let mut freed = 0u64;
    for entry in collect_files(&self.base)? {
      let relative = entry.strip_prefix(&self.base).map_err(|_| {
        StoreError::Invalid(format!(
          "blob path {} is outside {}",
          entry.display(),
          self.base.display()
        ))
      })?;
      let relative = relative.to_string_lossy().replace('\\', "/");
      // Liveness is compared in the durable form recorded in trace lines,
      // `blobs/<shard>/<hash>`, because that is the only form a caller can
      // extract from a journal. Comparing the bare file name instead would
      // silently delete payloads that are still referenced.
      let durable = format!("blobs/{relative}");
      if referenced.contains(&durable) || relative.contains(".part-") {
        continue;
      }
      let size = fs::metadata(&entry).map(|m| m.len()).unwrap_or(0);
      fs::remove_file(&entry)?;
      freed += size;
    }
    Ok(freed)
  }

  /// Total payload bytes held by this store.
  pub fn bytes(&self) -> Result<u64, StoreError> {
    let mut total = 0u64;
    for entry in collect_files(&self.base)? {
      total += fs::metadata(&entry).map(|m| m.len()).unwrap_or(0);
    }
    Ok(total)
  }
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<(), StoreError> {
  if let Some(parent) = path.parent() {
    fs::File::open(parent)?.sync_all()?;
  }
  Ok(())
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> Result<(), StoreError> {
  Ok(())
}

fn verify_file(path: &Path, blob: &BlobRef) -> Result<(), StoreError> {
  let encoded = fs::read(path)?;
  let bytes = decode(&encoded, blob.compression)?;
  if sha256_hex(&bytes) != blob.hash || bytes.len() as u64 != blob.size {
    let _ = fs::remove_file(path);
    return Err(StoreError::Invalid(format!(
      "temporary blob {} does not match its reference",
      path.display()
    )));
  }
  Ok(())
}

fn encode(bytes: &[u8], compression: BlobCompression) -> Result<Vec<u8>, StoreError> {
  match compression {
    BlobCompression::None => Ok(bytes.to_vec()),
    BlobCompression::Deflate => deflate(bytes),
  }
}

fn deflate(bytes: &[u8]) -> Result<Vec<u8>, StoreError> {
  let mut encoder = DeflateEncoder::new(Vec::new(), FlateCompression::default());
  encoder.write_all(bytes)?;
  encoder
    .finish()
    .map_err(|error| StoreError::Invalid(format!("cannot finish deflate payload: {error}")))
}

fn decode(bytes: &[u8], compression: BlobCompression) -> Result<Vec<u8>, StoreError> {
  match compression {
    BlobCompression::None => Ok(bytes.to_vec()),
    BlobCompression::Deflate => {
      let mut decoder = DeflateDecoder::new(bytes);
      let mut decoded = Vec::new();
      decoder
        .read_to_end(&mut decoded)
        .map_err(|error| StoreError::Invalid(format!("cannot decode deflate payload: {error}")))?;
      Ok(decoded)
    }
  }
}

fn collect_files(dir: &Path) -> Result<Vec<PathBuf>, StoreError> {
  let mut out = Vec::new();
  if !dir.exists() {
    return Ok(out);
  }
  for entry in fs::read_dir(dir)? {
    let entry = entry?;
    let file_type = entry.file_type()?;
    if file_type.is_dir() {
      out.extend(collect_files(&entry.path())?);
    } else {
      out.push(entry.path());
    }
  }
  Ok(out)
}

#[cfg(test)]
mod tests {
  use pi_rs_core::event::{AgentEvent, Diagnostic, DiagnosticLevel, EventEnvelope, EventMeta};
  use pi_rs_core::ids::{SessionId, TraceId};
  use pi_rs_core::trace::{ExternalizedField, TraceEntry};

  use crate::tmp::TempDir;

  use super::*;

  fn store(tmp: &TempDir) -> BlobStore {
    let layout = StateLayout::new(tmp.path());
    layout.create().unwrap();
    BlobStore::for_session(&layout, &SessionId::from_string("018f-blobs".to_string())).unwrap()
  }

  fn compressed_store(tmp: &TempDir) -> BlobStore {
    let layout = StateLayout::new(tmp.path());
    layout.create().unwrap();
    BlobStore::for_session_with_compression(
      &layout,
      &SessionId::from_string("018f-blobs".to_string()),
      BlobCompression::Deflate,
    )
    .unwrap()
  }

  #[test]
  fn put_is_content_addressed_and_deduplicated() {
    let tmp = TempDir::new("blob-put");
    let store = store(&tmp);
    let first = store.put(b"payload bytes", Some("text/plain")).unwrap();
    assert_eq!(first.size, 13);
    assert_eq!(first.hash, sha256_hex(b"payload bytes"));
    assert!(store.exists(&first));
    assert_eq!(store.bytes().unwrap(), 13);

    // Same bytes, different declared content type: content wins, no rewrite.
    let again = store.put(b"payload bytes", None).unwrap();
    assert_eq!(again.hash, first.hash);
    assert_eq!(
      again.content_type, None,
      "reference is derived from bytes only"
    );
    assert_eq!(store.bytes().unwrap(), 13, "dedup must not store twice");
    assert_eq!(store.get(&first).unwrap(), b"payload bytes");
    assert!(store.verify(&first).unwrap());
  }

  #[test]
  fn compression_is_opt_in_and_references_logical_bytes() {
    let tmp = TempDir::new("blob-compression");
    let store = compressed_store(&tmp);
    let bytes = b"repeated payload ".repeat(4 * 1024);
    let blob = store.put(&bytes, Some("text/plain")).unwrap();

    assert_eq!(blob.compression, BlobCompression::Deflate);
    assert!(blob.relative_path().ends_with(".deflate"));
    assert_eq!(blob.size, bytes.len() as u64);
    assert_eq!(blob.hash, sha256_hex(&bytes));
    assert_eq!(store.get(&blob).unwrap(), bytes);
    assert_eq!(store.get_relative(&blob.relative_path()).unwrap(), bytes);
    assert!(store.verify(&blob).unwrap());
    assert!(fs::metadata(store.path_for(&blob)).unwrap().len() < blob.size);
  }

  #[test]
  fn incompressible_payload_falls_back_to_the_legacy_raw_reference() {
    let tmp = TempDir::new("blob-compression-fallback");
    let store = compressed_store(&tmp);
    let mut bytes = Vec::with_capacity(16 * 1024);
    let mut state = 0x1234_5678_u32;
    for _ in 0..16 * 1024 {
      state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
      bytes.push((state >> 24) as u8);
    }
    let blob = store.put(&bytes, None).unwrap();

    assert_eq!(blob.compression, BlobCompression::None);
    assert!(!blob.relative_path().contains(".deflate"));
    assert_eq!(store.get(&blob).unwrap(), bytes);
  }

  #[test]
  fn compressed_store_reads_legacy_raw_references() {
    let tmp = TempDir::new("blob-compression-legacy");
    let layout = StateLayout::new(tmp.path());
    layout.create().unwrap();
    let session = SessionId::from_string("018f-blobs".to_string());
    let raw = BlobStore::for_session(&layout, &session).unwrap();
    let bytes = b"legacy payload";
    let blob = raw.put(bytes, None).unwrap();
    let compressed =
      BlobStore::for_session_with_compression(&layout, &session, BlobCompression::Deflate).unwrap();

    assert_eq!(compressed.get(&blob).unwrap(), bytes);
    assert_eq!(
      compressed.get_relative(&blob.relative_path()).unwrap(),
      bytes
    );
    assert!(compressed.verify(&blob).unwrap());
  }

  #[test]
  fn corrupted_compressed_payload_is_rejected() {
    let tmp = TempDir::new("blob-compression-corrupt");
    let store = compressed_store(&tmp);
    let blob = store.put(&b"repeated payload ".repeat(128), None).unwrap();
    fs::write(store.path_for(&blob), b"not-deflate").unwrap();

    assert!(!store.verify(&blob).unwrap());
    assert!(matches!(
      store.get(&blob),
      Err(StoreError::Invalid(message)) if message.contains("deflate")
    ));
  }

  #[test]
  fn references_resolve_through_the_sharded_relative_path() {
    let tmp = TempDir::new("blob-ref");
    let store = store(&tmp);
    let blob = store.put(b"tool output", None).unwrap();
    let shard = &blob.hash[..2];
    assert!(
      store
        .path_for(&blob)
        .ends_with(format!("{shard}/{}", blob.hash)),
      "payloads must be sharded by hash prefix: {}",
      store.path_for(&blob).display()
    );
    assert!(store.get_text(&blob).unwrap().contains("tool output"));
    let record = serde_json::json!({ "blob": blob.relative_path() });
    assert_eq!(record["blob"], blob.relative_path());
    assert!(blob.recovery_ref().ends_with(&blob.short_hash()));
  }

  #[test]
  fn torn_objects_never_reach_the_final_path() {
    let tmp = TempDir::new("blob-torn");
    let store = store(&tmp);
    let blob = BlobRef::for_bytes(b"real", None);
    let path = store.path_for(&blob);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    // Simulate a writer that was killed mid-write.
    fs::write(&path, "partial").unwrap();
    assert!(
      !store.verify(&blob).unwrap(),
      "a stored object whose bytes disagree with its hash must be detected"
    );
    // A subsequent put repairs it rather than trusting the existing file.
    let stored = store.put(b"real", None).unwrap();
    assert_eq!(stored.hash, blob.hash);
    assert!(store.verify(&blob).unwrap());
  }

  #[test]
  fn missing_payloads_are_missing_not_empty() {
    let tmp = TempDir::new("blob-missing");
    let store = store(&tmp);
    let blob = BlobRef::for_bytes(b"never stored", None);
    assert!(!store.exists(&blob));
    assert!(matches!(store.get(&blob), Err(StoreError::Missing(_))));
    assert!(!store.verify(&blob).unwrap());
  }

  #[test]
  fn binary_payloads_do_not_become_text() {
    let tmp = TempDir::new("blob-binary");
    let store = store(&tmp);
    let blob = store.put(&[0xff, 0xfe, 0x00], None).unwrap();
    assert!(matches!(store.get_text(&blob), Err(StoreError::Invalid(_))));
    assert_eq!(store.get(&blob).unwrap(), vec![0xff, 0xfe, 0x00]);
  }

  #[test]
  fn prune_keeps_everything_any_record_mentions() {
    let tmp = TempDir::new("blob-prune");
    let store = store(&tmp);
    let kept = store.put(b"kept", None).unwrap();
    let dropped = store.put(b"dropped", None).unwrap();
    assert_eq!(store.bytes().unwrap(), 11, "kept (4) + dropped (7)");

    // A trace line that mentions the kept blob is the liveness evidence.
    let entry = TraceEntry {
      envelope: EventEnvelope::new(
        EventMeta::new(SessionId::new(), TraceId::new()),
        AgentEvent::Diagnostic(Diagnostic {
          level: DiagnosticLevel::Info,
          message: "kept".into(),
        }),
      ),
      redactions: 0,
      raw_payload: false,
      raw_ref: Some(kept.relative_path()),
      externalized: Vec::new(),
    };
    let line = serde_json::to_string(&entry).unwrap();
    let referenced: std::collections::BTreeSet<String> = collect_strings(&line)
      .into_iter()
      .filter(|text| text.starts_with("blobs/"))
      .collect();
    assert!(referenced.contains(&kept.relative_path()));
    assert_eq!(
      store.prune_unreferenced(&referenced).unwrap(),
      7,
      "only the unreferenced payload's bytes are freed"
    );
    assert!(store.exists(&kept));
    assert!(!store.exists(&dropped));
    assert_eq!(store.bytes().unwrap(), 4);
  }

  #[test]
  fn a_recorded_reference_can_be_followed_without_a_blob_object() {
    let tmp = TempDir::new("blob-relative-read");
    let store = store(&tmp);
    let blob = store.put(b"bounded payload", None).unwrap();
    assert_eq!(
      store.get_relative(&blob.relative_path()).unwrap(),
      b"bounded payload"
    );
  }

  #[test]
  fn a_reference_that_is_not_the_layout_shape_is_refused() {
    let tmp = TempDir::new("blob-relative-escape");
    let store = store(&tmp);
    for forged in [
      "../../secrets",
      "blobs/..%2f/secrets",
      "blobs/../abcd",
      "blobs/zz/deadbeef",
      "blobs/0e/not-a-hash",
      "blobs/0e",
      "sessions/0e/abcd",
      "blobs/0e/000000000000000000000000000000000000000000000000000000000000000z",
    ] {
      assert!(
        matches!(store.get_relative(forged), Err(StoreError::Invalid(_))),
        "{forged} must not be joined onto the store root"
      );
    }
    // A well-formed reference to bytes that are not stored is a miss, not a panic.
    let absent = BlobRef::for_bytes(b"never stored", None);
    assert!(matches!(
      store.get_relative(&absent.relative_path()),
      Err(StoreError::Missing(_))
    ));
  }

  /// A spilled field leaves two traces of its reference, and liveness scanning
  /// reads both: the structured record, whose value is exactly
  /// `blobs/<shard>/<hash>`, and the preview marker naming the same path inside
  /// prose. A collector that understood only the first would still keep the blob;
  /// one that understood neither would delete bytes a line is still pointing at.
  #[test]
  fn a_reference_written_by_payload_bounding_is_visible_to_liveness_scanning() {
    let tmp = TempDir::new("blob-bounded-ref");
    let store = store(&tmp);
    let spilled = store.put(b"bounded payload", None).unwrap();
    let entry = TraceEntry {
      envelope: EventEnvelope::new(
        EventMeta::new(SessionId::new(), TraceId::new()),
        AgentEvent::Diagnostic(Diagnostic {
          level: DiagnosticLevel::Info,
          message: format!(
            "head of a long payload\u{2026} [stored 40960 bytes in {}, 256 bytes shown]",
            spilled.relative_path()
          ),
        }),
      ),
      redactions: 0,
      raw_payload: false,
      raw_ref: None,
      externalized: vec![ExternalizedField {
        field: "output".into(),
        reference: spilled.relative_path(),
        bytes: 40960,
        inline: 320,
      }],
    };
    let line = serde_json::to_string(&entry).unwrap();
    let referenced: std::collections::BTreeSet<String> = collect_strings(&line)
      .into_iter()
      .filter(|text| text.starts_with("blobs/"))
      .collect();
    assert!(
      referenced.contains(&spilled.relative_path()),
      "the structured record is a whole string, so it is collected: {referenced:?}"
    );
    let bytes = b"nobody points here";
    let unreferenced = store.put(bytes, None).unwrap();
    assert_eq!(
      store.prune_unreferenced(&referenced).unwrap(),
      bytes.len() as u64
    );
    assert!(store.exists(&spilled), "a referenced payload survives");
    assert!(!store.exists(&unreferenced));
  }

  /// Collect quoted strings from a JSON line, for reference-liveness tests.
  fn collect_strings(line: &str) -> Vec<String> {
    serde_json::from_str::<serde_json::Value>(line)
      .map(|value| {
        let mut out = Vec::new();
        collect(&value, &mut out);
        out
      })
      .unwrap_or_default()
  }

  fn collect(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
      serde_json::Value::String(text) => out.push(text.clone()),
      serde_json::Value::Array(items) => {
        for item in items {
          collect(item, out);
        }
      }
      serde_json::Value::Object(map) => {
        for item in map.values() {
          collect(item, out);
        }
      }
      _ => {}
    }
  }
}
