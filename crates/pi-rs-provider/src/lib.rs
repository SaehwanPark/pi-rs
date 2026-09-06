//! Provider adapters for pi-rs.
//!
//! The adapter boundary is where a provider's dialect becomes the harness's
//! [`pi_rs_core::ModelProvider`] contract. Everything above it — the turn loop,
//! the context policy, the TUI — is supposed to be provider-agnostic, so the
//! adapter owns exactly three jobs:
//!
//! 1. **Mapping.** Harness request state to a wire request, with explicit,
//!    tested switches for the dialect differences between implementations that
//!    all claim to be "OpenAI compatible".
//! 2. **Provenance-correct streaming.** Thinking text is emitted as reasoning
//!    with its true provenance, never promoted, never inferred, and never
//!    silently converted into visible text.
//! 3. **Failure normalization.** Transport, HTTP, and decoding failures become
//!    a typed [`pi_rs_core::ModelFailure`] with an honest phase and
//!    `partial_output_emitted`, because retry and failover decisions read those
//!    fields directly.
//!
//! There is deliberately no discovery layer here: capabilities are declared in
//! configuration and gaps are reported, not guessed from a live probe.

pub mod config;
pub mod decode;
pub mod deferred;
pub mod mapping;
pub mod openai;
pub mod sse;

pub use config::{BuildError, DEFAULT_BASE_URL, MaxTokensField, ProviderConfig, ThinkingInput};
pub use decode::Decoder;
pub use deferred::Deferred;
pub use mapping::{request_body, serves};
pub use openai::OpenAiCompat;
pub use sse::{DONE, MAX_EVENT_BYTES, SseEvent, SseStream};
