//! Durable state for `pi-rs`: session logs, canonical trace journals, checkpoint
//! capsules, content-addressed blobs, and bounded retention.
//!
//! The crate exists so that the runtime can announce facts without deciding how
//! they are persisted. Three properties are enforced here rather than at call
//! sites, because enforcement at call sites would be optional:
//!
//! - **Redaction.** Every durable line passes through the redaction policy at
//!   the write boundary. Trace data may contain secrets; "we sanitize on the way
//!   out" is only true if there is exactly one way out.
//! - **Ordering.** Sequence numbers are assigned by the journal, not by
//!   producers, and are recovered from the journal tail on reopen. A session can
//!   therefore be resumed without hydrating it.
//! - **Two representations of the same session.** `sessions/<id>.jsonl` holds
//!   semantic state that resume needs; `sessions/<id>.trace.jsonl` holds the
//!   canonical, high-resolution history. Neither is derived from the other at
//!   read time, which is why resume is cheap and the trace stays complete.
//!
//! All I/O is blocking and bounded. Nothing in this crate scans a directory
//! eagerly, parses a whole journal to answer a metadata question, or creates a
//! task. Startup calls [`Store::open`] once; everything else is on demand.
//!
//! ```text
//! Store::open(root, WritePolicy)      one bounded mkdir pass
//!   ├─ begin / resume                 -> Session (durable handle)
//!   │     ├─ emit(&mut envelope)      -> EventSeq, stamped back into the envelope
//!   │     ├─ append_message(..)       -> session line pointing at that seq
//!   │     └─ checkpoint(&capsule)     -> capsule file + barrier line
//!   ├─ restore(id)                    -> latest checkpoint + records after it
//!   ├─ summaries(limit)               -> headers + trace tails only
//!   └─ plan_retention / apply_retention
//! ```

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

mod blob;
mod error;
mod journal;
mod jsonl;
mod layout;

mod retention;
mod session_log;
mod store;
pub mod tmp;

/// Reading a Pi session file. Exported as a module: the importer is a surface, not a detail
/// of the store's own formats.
pub mod pi_import;

pub use blob::BlobStore;
pub use error::StoreError;
pub use journal::{TraceJournal, requires_durable_write};
pub use jsonl::{LineWriter, ReadReport, read_first_line, read_jsonl, read_jsonl_tail};
pub use layout::StateLayout;
pub use retention::{DEFAULT_KEEP_NEWEST, RetentionReport, session_started_ms};
pub use session_log::{RestoredSession, SessionLog};
pub use store::{DEFAULT_INLINE_THRESHOLD_BYTES, Payload, Session, Store, WritePolicy};
pub use tmp::TempDir;

/// Schema version of this crate's on-disk layout.
///
/// Recorded separately from the record-level schema versions so that a layout
/// change (paths, directory shape) is distinguishable from a record change.
pub const LAYOUT_VERSION: u32 = 1;

#[cfg(test)]
mod tests {
  use pi_rs_core::event::AgentEvent;

  use super::*;

  #[test]
  fn layout_version_is_reported() {
    assert_eq!(LAYOUT_VERSION, 1);
  }

  #[test]
  fn durable_write_rule_excludes_only_streaming_deltas() {
    use pi_rs_core::event::{AssistantDelta, ReasoningDelta, TurnCompleted, TurnStatus};
    use pi_rs_core::provenance::ReasoningProvenance;

    assert!(!requires_durable_write(&AgentEvent::AssistantDelta(
      AssistantDelta {
        text: "tok".into(),
        chunk_index: 0,
      }
    )));
    assert!(!requires_durable_write(&AgentEvent::ReasoningDelta(
      ReasoningDelta {
        text: "think".into(),
        provenance: ReasoningProvenance::Native,
        chunk_index: 0,
      }
    )));
    assert!(requires_durable_write(&AgentEvent::TurnCompleted(
      TurnCompleted {
        status: TurnStatus::Completed,
        duration_ms: 1,
      }
    )));
  }
}
