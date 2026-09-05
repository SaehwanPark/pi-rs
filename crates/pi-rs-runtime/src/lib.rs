//! `pi-rs-runtime` — the turn loop.
//!
//! This crate answers the one question that cannot be answered anywhere else:
//! **which model owns this turn, and what happened to it.** Everything else is
//! already decided elsewhere — capabilities in `core`, bytes in `store`, wire
//! format in `provider`, side effects in `tools`. The runtime sequences them, and
//! the sequence is the product:
//!
//! ```text
//! user message
//!   -> context policy decision (may reduce first)
//!   -> model request, streamed as typed events
//!   -> tool calls executed under policy, each with a durable lifecycle state
//!   -> results fed back, loop until the model stops asking
//!   -> turn completion, with the reason
//! ```
//!
//! Two properties are defended here rather than trusted:
//!
//! - **Every state transition is an event.** Nothing is recorded only in a log
//!   line, because a transcript that cannot be replayed cannot be trusted.
//! - **Uncertainty is terminal.** A tool whose completion was not observed stays
//!   `Unknown`; a partially-streamed response is never silently restarted. The
//!   runtime does not upgrade its own ignorance into a success or a failure.

#![forbid(unsafe_code)]
// The turn error intentionally carries a full normalized failure; see `TurnError`.
#![allow(clippy::result_large_err)]

pub mod failover;
pub mod store_trace;
pub mod turn;

pub use failover::{FailoverPolicy, Recovery};
pub use store_trace::StoreTrace;
pub use turn::{
  MAX_MODEL_REQUESTS_PER_TURN, SilentProgress, Trace, TraceSink, TurnError, TurnLoop, TurnProgress,
  TurnReport,
};
