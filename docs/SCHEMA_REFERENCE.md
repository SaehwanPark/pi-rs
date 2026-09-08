# Schema reference: events, session records, provenance

This document describes the schemas **as the code defines them today**. It was written
from `grep` output and `Read`, not from the slice spec's recollection. Where the slice
spec (`docs/SLICE_SCHEMAS.md`) disagreed with the repository, the repository won and the
mismatch is recorded in place.

## Method and scope

* Variants were enumerated with `grep -n` against the defining file; every variant below
  appears in that output, and no variant appears below that grep did not print.
* **Producers** is a production-code construction check. `#[cfg(test)]` modules were cut
  from each `.rs` file under `crates/` and the root `src/`, then construction sites were
  matched with:

  ```sh
  # <Variant> is the variant name, e.g. ToolCompleted
  grep -rEn "AgentEvent::<Variant>\((pi_rs_core::)?<Variant> *\\{" --include=*.rs <stripped tree>
  ```

  `yes (file:line)` is one production construction site; **`none found`** means no
  production code builds that variant (match arms, re-exports, and doc comments do not
  count). Every file has at most one `#[cfg(test)]`, followed by `mod tests {` or
  `pub(crate) mod testutil {`, so the cut cannot drop production code.
* `tests/`, `bench/`, and `#[cfg(test)]` code are excluded from "production".

**Not in scope** (explicitly, per the slice): no schema changes, no new event variants, no
serde attribute changes, no fixes to the zero-producer variants listed below, and no
`ROADMAP.md` tick — `ROADMAP.md:57` stays `- [ ] Event/session/provenance schemas are
documented.` until a human confirms this document is complete.

## 1. Events

Source of truth: `crates/pi-rs-core/src/event.rs`.

### 1.1 Envelope and sequencing contract

Every durable event line is an `EventEnvelope` (`event.rs:110`):

| Field | Type | Notes |
| --- | --- | --- |
| `v` | `u32` | stamped from `EVENT_SCHEMA_VERSION: u32 = 1` (`event.rs:44`) |
| `meta` | `EventMeta` | identity and ordering, below |
| `event` | `AgentEvent` | `#[serde(flatten)]` (`event.rs:113`) |

`AgentEvent` is internally tagged `#[serde(rename_all = "snake_case", tag = "type")]`
(`event.rs:162`), so one journal line is
`{"v":1,"meta":{…},"type":"<variant>","<payload fields…>"}`.

`EventMeta` (`event.rs:48`) is carried by every event:

| Field | Type |
| --- | --- |
| `event_id` | `EventId` |
| `session_id` | `SessionId` |
| `turn_id` | `Option<TurnId>` |
| `seq` | `Option<EventSeq>` — *"Assigned by the durable log. `None` only while an event is in flight."* (`event.rs:53`) |
| `timestamp_ms` | `u64` |
| `model_epoch` | `Option<u32>` |
| `model` | `Option<ModelRef>` |
| `tool_call_id` | `Option<ToolCallId>` |
| `parent_event_id` | `Option<EventId>` |
| `trace_id` | `TraceId` |
| `span_id` | `crate::ids::SpanId` |

All `Option` fields above except `seq` carry `#[serde(default, skip_serializing_if =
"Option::is_none")]`, so identity fields are absent from a line only where the fact does
not exist yet.

The sequencing contract, as the module states it (`event.rs:12-15`):

> **Ordering.** Within one session, ordering is defined by [`EventSeq`], assigned by the
> durable event log at append time. Timestamps are for humans and timelines; they never
> define order, because clocks step.

* `EventSeq` is `pub struct EventSeq(pub u64)` (`ids.rs:83`), `#[serde(transparent)]`
  (`ids.rs:82`); its doc repeats the rule: *"Ordering claims always use `seq`, never
  timestamps, because clocks may step backwards."* (`ids.rs:79-80`)
* The log, not the producer, assigns it:
  `let seq = EventSeq(self.last_seq.map(|seq| seq.0 + 1).unwrap_or(1));`
  (`crates/pi-rs-store/src/journal.rs:105`), written into the envelope at
  `journal.rs:113` (`entry.envelope.meta.seq = Some(seq);`) and remembered at
  `journal.rs:125`. Sequence numbers therefore start at `1`.
* `Store::emit` (`crates/pi-rs-store/src/store.rs:338`) returns that journal sequence and
  stamps it back into the caller's envelope, which is why `append_message` rejects an
  envelope with `seq == None` (`store.rs:387-393`, `let seq = envelope.meta.seq
  .ok_or_else(|| {`): a session line may only point at a real journal position.
* `timestamp_ms` comes from `now_millis()` (`ids.rs:24`), the platform clock, called in
  `EventMeta::new` (`event.rs:76`). The clock is explicitly not assumed monotonic.
* `redactions` is added to the journal line by the store's redaction policy only when
  redactions occurred (`journal.rs:116-118`); it is a store-side field and is **not** part
  of `EventEnvelope`.

### 1.2 `AgentEvent` variants

Enumerated with:

```sh
awk 'NR>=163 && NR<=281 && /^  [A-Z][A-Za-z0-9_]*\(/ {print NR": "$0}' \
  crates/pi-rs-core/src/event.rs
```

22 variants. Wire tag is the snake_case variant name (`event.rs:162`).

#### `session_started` — `AgentEvent::SessionStarted` (`event.rs:168`), payload `event.rs:284`

Purpose: the session exists and which model owns the first epoch.
Fields: `working_dir: String`, `model: ModelRef`, `capabilities: ModelCapabilities`,
`resumed: bool` (`#[serde(default)]`).
Producers: yes (`crates/pi-rs-runtime/src/turn.rs:520`)

#### `user_message` — `AgentEvent::UserMessage` (`event.rs:173`), payload `event.rs:294`

Purpose: a user message was accepted into canonical history.
Fields: `text: String`, `attachments: u32` (`#[serde(default)]`).
Producers: yes (`crates/pi-rs-runtime/src/turn.rs:408`)

#### `model_request_started` — `AgentEvent::ModelRequestStarted` (`event.rs:178`), payload `event.rs:303`

Purpose: a model request began, which is the boundary for partial output.
Fields: `epoch: u32`, `model: ModelRef`, `message_count: u32`,
`context_tokens_est: u64` (estimate, not a measurement), `tools_exposed: u32`.
Producers: yes (`crates/pi-rs-runtime/src/turn.rs:643`)

#### `reasoning_delta` — `AgentEvent::ReasoningDelta` (`event.rs:184`), payload `event.rs:314`

Purpose: reasoning-like text arrived, with its provenance claim.
Fields: `text: String`, `provenance: ReasoningProvenance`, `chunk_index: u32`.
Producers: yes (`crates/pi-rs-runtime/src/turn.rs:1329`)

#### `assistant_delta` — `AgentEvent::AssistantDelta` (`event.rs:190`), payload `event.rs:321`

Purpose: assistant prose arrived.
Fields: `text: String`, `chunk_index: u32`.
Producers: yes (`crates/pi-rs-runtime/src/turn.rs:1342`)

#### `model_request_completed` — `AgentEvent::ModelRequestCompleted` (`event.rs:195`), payload `event.rs:327`

Purpose: a model request finished and is attributable.
Fields: `epoch: u32`, `model: ModelRef`, `finish_reason: Option<String>`,
`input_tokens: Option<u64>`, `output_tokens: Option<u64>`, `duration_ms: u64`,
`tool_calls: u32`, `reasoning_provenance: Option<ReasoningProvenance>` (all `Option`
fields `#[serde(default, skip_serializing_if = "Option::is_none")]`).
Producers: yes (`crates/pi-rs-runtime/src/turn.rs:695`)

#### `model_retry` — `AgentEvent::ModelRetry` (`event.rs:200`), payload `event.rs:346`

Purpose: the same request is being retried against the same model.
Fields: `attempt: u32`, `max_attempts: u32`, `kind: ModelFailureKind`,
`retry_after_ms: Option<u64>`, `will_failover: bool` (`#[serde(default)]`).
Producers: yes (`crates/pi-rs-runtime/src/turn.rs:815`)

#### `model_failover` — `AgentEvent::ModelFailover` (`event.rs:205`), payload `event.rs:358`

Purpose: availability failure moved generation to the backup model.
Fields: `from: ModelRef`, `to: ModelRef`, `kind: ModelFailureKind`,
`gaps: Vec<CapabilityGap>` (`#[serde(default)]`), `compacted: bool` (`#[serde(default)]`).
Producers: yes (`crates/pi-rs-runtime/src/turn.rs:867`)

#### `model_epoch_started` — `AgentEvent::ModelEpochStarted` (`event.rs:211`), payload `event.rs:371`

Purpose: which model owned a span of generation, and what it was believed capable of at
that moment.
Fields: `epoch: u32`, `model: ModelRef`, `reason: EpochReason`,
`capabilities: ModelCapabilities`.
Producers: yes (`crates/pi-rs-runtime/src/turn.rs:530`)

#### `tool_requested` — `AgentEvent::ToolRequested` (`event.rs:216`), payload `event.rs:379`

Purpose: the model asked for a tool call and arguments are fully decoded.
Fields: `call_id: ToolCallId`, `name: String`, `arguments: serde_json::Value`,
`read_only: bool` (`#[serde(default)]`).
Producers: yes (`crates/pi-rs-runtime/src/turn.rs:1014`)

#### `tool_started` — `AgentEvent::ToolStarted` (`event.rs:221`), payload `event.rs:388`

Purpose: execution began, so a later crash has an observed boundary.
Fields: `call_id: ToolCallId`, `name: String`.
Producers: yes (`crates/pi-rs-runtime/src/turn.rs:1110`)

#### `tool_completed` — `AgentEvent::ToolCompleted` (`event.rs:226`), payload `event.rs:394`

Purpose: the call completed with a committed result.
Fields: `call_id: ToolCallId`, `name: String`, `state: ToolExecutionState` (*"Always
[`ToolExecutionState::Succeeded`]; kept explicit so that the journal states the claim
instead of implying it."*), `duration_ms: u64`, `status: Option<i64>`, `reduced: bool`
(`#[serde(default)]`), `blob: Option<BlobRef>`, `visible_bytes: u64`.
Producers: yes (`crates/pi-rs-runtime/src/turn.rs:1175`)

#### `tool_failed` — `AgentEvent::ToolFailed` (`event.rs:231`), payload `event.rs:414`

Purpose: the call completed with an observed failure.
Fields: `call_id: ToolCallId`, `name: String`, `message: String`, `duration_ms: u64`,
`status: Option<i64>`.
Producers: yes (`crates/pi-rs-runtime/src/turn.rs:1023`)

#### `tool_unknown` — `AgentEvent::ToolUnknown` (`event.rs:237`), payload `event.rs:424`

Purpose: completion could not be observed, which is not the same as failure.
Fields: `call_id: ToolCallId`, `name: String`, `why: String`, `mutating: bool`.
Producers: yes (`crates/pi-rs-runtime/src/turn.rs:1196`)

#### `external_context_retrieved` — `AgentEvent::ExternalContextRetrieved` (`event.rs:242`), payload `event.rs:435`

Purpose: external knowledge entered context, with citation and provenance.
Fields: `source: ExternalContextSource`, `citation: Option<String>`, `bytes: u64`,
`inline: bool`.
Producers: **none found**. Production code only consumes it
(`crates/pi-rs-tui/src/transcript.rs:384`) and re-exports it
(`crates/pi-rs-core/src/lib.rs:56`). See §1.4.

#### `context_reduced` — `AgentEvent::ContextReduced` (`event.rs:248`), payload `event.rs:445`

Purpose: model-visible payload was reduced to a bounded representation.
Fields: `reason: ReductionReason`, `original_bytes: u64`, `visible_bytes: u64`,
`blob: BlobRef`, `recovery_ref: String`, `tool_call_id: Option<ToolCallId>`.
Producers: yes (`crates/pi-rs-runtime/src/turn.rs:921`)

#### `context_compaction_started` — `AgentEvent::ContextCompactionStarted` (`event.rs:253`), payload `event.rs:458`

Purpose: compaction began at a safe boundary.
Fields: `level: ContextLevel`, `reason: String`.
Producers: **none found**. Consumed at `crates/pi-rs-tui/src/transcript.rs:419`. See §1.4.

#### `context_compaction_completed` — `AgentEvent::ContextCompactionCompleted` (`event.rs:258`), payload `event.rs:464`

Purpose: compaction finished and what it retained.
Fields: `level: ContextLevel`, `removed_messages: u32`, `retained_messages: u32`,
`context_epoch: u32`.
Producers: **none found**. Consumed at `crates/pi-rs-tui/src/transcript.rs:426`. See §1.4.

#### `checkpoint_created` — `AgentEvent::CheckpointCreated` (`event.rs:263`), payload `event.rs:472`

Purpose: an episode checkpoint capsule was written.
Fields: `checkpoint_id: CheckpointId`, `capsule_version: u32`,
`summarized_events: u64`, `path: String`.
Producers: **none found**. Consumed at `crates/pi-rs-tui/src/transcript.rs:445`; the store
documents that the caller still owes the event
(`crates/pi-rs-store/src/store.rs:409`). See §1.4.

#### `turn_completed` — `AgentEvent::TurnCompleted` (`event.rs:268`), payload `event.rs:480`

Purpose: a turn ended and how.
Fields: `status: TurnStatus`, `duration_ms: u64`.
Producers: yes (`crates/pi-rs-runtime/src/turn.rs:601`)

#### `diagnostic` — `AgentEvent::Diagnostic` (`event.rs:275`), payload `event.rs:486`

Purpose: an operator-visible condition that is not a domain event.
Fields: `level: DiagnosticLevel`, `message: String`.
Producers: yes (`crates/pi-rs-runtime/src/turn.rs:582`)

#### `session_ended` — `AgentEvent::SessionEnded` (`event.rs:280`), payload `event.rs:492`

Purpose: the session ended.
Fields: `reason: SessionEndReason`.
Producers: yes (`crates/pi-rs-runtime/src/turn.rs:493`)

### 1.3 Supporting enums carried by payloads

All three are `#[serde(rename_all = "snake_case")]` and carry no tag.

* `SessionEndReason` (`event.rs:130`): `UserExit` (`:132`), `Restart` (`:134`),
  `Fatal { message: String }` (`:136`).
* `TurnStatus` (`event.rs:142`): `Completed` (`:143`), `Cancelled` (`:144`),
  `Failed { kind: ModelFailureKind }` (`:145`).
* `DiagnosticLevel` (`event.rs:151`): `Info` (`:152`), `Warn` (`:153`), `Error` (`:154`).

`AttributedMessage` (`event.rs:499`) is a struct, not an event variant:
`envelope: EventEnvelope`, `message: Message`, used by session reconstruction.

### 1.4 Variants with zero production producers

Four of the 22 variants are never constructed by production code:

| Variant | `AgentEvent::…` mentions outside `#[cfg(test)]` |
| --- | --- |
| `external_context_retrieved` | none |
| `context_compaction_started` | none |
| `context_compaction_completed` | none |
| `checkpoint_created` | none |

This is a documentation finding, not a fix. `docs/SLICE_SCHEMAS.md:19` says Issue #38
already found three compaction variants with zero producers; that count matches the
compaction/checkpoint trio here (`context_compaction_started`,
`context_compaction_completed`, `checkpoint_created`), and `external_context_retrieved` is
a fourth zero-producer variant. The state still stands; nothing was changed.
