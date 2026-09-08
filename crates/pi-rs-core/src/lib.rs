//! Core runtime contracts for `pi-rs`.
//!
//! This crate defines the vocabulary of the system and nothing else: the
//! canonical event log, session records, context policy, model capabilities,
//! failure taxonomy, tool lifecycle state, reasoning provenance, redaction,
//! and trust gating. It performs no I/O, talks to no provider, and renders
//! nothing, which is what lets the runtime, storage, providers, and UI stay
//! independent and independently testable.
//!
//! Design rules enforced here:
//!
//! - **Provenance is explicit.** Reasoning-like text always carries whether it
//!   was native, provider-summarized, declared, or reconstructed.
//! - **Ordering is structural.** [`event::EventEnvelope`] carries a monotonic
//!   session sequence; producers and consumers never rely on wall-clock order.
//! - **Unknown is not failure.** [`tool::ToolExecutionState::Unknown`] and
//!   [`failure::CompletionCertainty::Unknown`] are first-class and survive persistence.
//! - **Policy is data.** Failover, replay, and context decisions are pure
//!   functions over typed inputs, so their semantics are unit-testable.
//! - **Secrets are guarded at the boundary.** Durable text passes through
//!   [`redact::RedactionPolicy`] at the store/trace boundary, not at call sites.

#![forbid(unsafe_code)]

pub mod capability;
pub mod config;
pub mod context;
pub mod event;
pub mod failure;
pub mod hash;
pub mod ids;
pub mod message;
pub mod provenance;
pub mod provider;
pub mod redact;
pub mod session;
pub mod sink;
pub mod tool;
pub mod trace;
pub mod trust;

pub use capability::{
  CapabilityGap, EpochReason, ModelAttribution, ModelCapabilities, ModelEpoch, ModelRef,
  ReasoningExposure,
};
pub use config::{
  ConfigError, ContextOverrides, ModelEndpoint, RuntimeConfig, ToolPolicy, UiConfig,
};
pub use context::{
  CapsuleArtifact, CapsuleDecision, ContextAction, ContextCapsule, ContextDecision, ContextLevel,
  ContextPolicy, ContextProfile, ContextState, ContextThresholds, ProfilePolicy, ReductionReason,
};
pub use event::{
  AgentEvent, AssistantDelta, AttributedMessage, CheckpointCreated, ContextCompactionCompleted,
  ContextCompactionEpoch, ContextCompactionStarted, ContextReduced, Diagnostic, DiagnosticLevel,
  EventEnvelope, EventMeta, ExternalContextRetrieved, FIRST_COMPACTION_EPOCH, ModelEpochStarted,
  ModelFailover, ModelRequestCompleted, ModelRequestStarted, ModelRetry, ReasoningDelta,
  SessionEndReason, SessionEnded, SessionStarted, ToolCompleted, ToolFailed, ToolRequested,
  ToolStarted, ToolUnknown, TurnCompleted, TurnStatus, UserMessage, next_context_epoch,
};
pub use failure::{CompletionCertainty, FailurePhase, ModelFailure, ModelFailureKind};
pub use ids::{
  CheckpointId, EventId, EventSeq, SessionId, SpanId, ToolCallId, TraceId, TurnId, now_millis,
  uuidv7,
};
pub use message::{ContentBlock, Message, Role, ToolCallBlock, ToolResultBlock};
pub use provenance::{ReasoningChunk, ReasoningProvenance};
pub use provider::{
  CancelToken, Collector, CompletionUsage, ModelProvider, ModelRequest, ProviderEvent,
  ProviderEventSink, ThinkingLevel, ToolSpec,
};
pub use redact::{Redacted, RedactionPolicy, SecretKind};
pub use session::{
  SessionCheckpointRecord, SessionCompactionRecord, SessionEpochRecord, SessionHeader,
  SessionMessage, SessionRecord, SessionSummary,
};
pub use sink::{EventSink, FanOut, MemorySink, NullSink, SinkError};
pub use tool::{
  ReplayDecision, Tool, ToolChunk, ToolError, ToolExecutionState, ToolMetadata, ToolOutcome,
  ToolProgress, ToolRequest,
};
pub use trace::{
  BlobRef, ExternalContextSource, RawPayloadCapture, TRACE_SCHEMA_VERSION, TraceEntry,
  TraceRetention,
};
pub use trust::{
  EmptyTrustStore, Risk, TrustDecision, TrustEntry, TrustGate, TrustScope, TrustStore,
};

/// Crate version of the core contract set, re-exported for diagnostics.
pub const CORE_VERSION: &str = env!("CARGO_PKG_VERSION");
