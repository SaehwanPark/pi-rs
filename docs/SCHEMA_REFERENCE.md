# Schema reference: events, session records, provenance

> **Current for v0.2.2 (2026-09-20).** This document describes the serialized schemas and
> verified production boundaries from the audited mainline. Source remains authoritative
> when a line reference changes; release-specific history belongs in `CHANGELOG.md` and
> `docs/archive/`.

It was written from the repository's source and producer inventory, not from a slice
spec's recollection. Where
the slice spec (`docs/archive/slices/SLICE_SCHEMAS.md`) disagreed with the repository at that time, the
repository won and the mismatch is recorded in place.

## Method and scope

* Variants were enumerated with `grep -n` against the defining file; every variant below
  appears in that output, and no variant appears below that grep did not print.
* **Producers** is a production-code construction check. `#[cfg(test)]` modules were cut
  from each `.rs` file under `crates/` and the root `src/`, then construction sites were
  matched with:

  ```sh
  # <Variant> is the variant name, e.g. ToolCompleted
  grep -rEn "AgentEvent::<Variant>\((rupi_core::)?<Variant> *\\{" --include=*.rs <stripped tree>
  ```

  `yes (file:line)` is one production construction site; **`none found`** means no
  production code builds that variant (match arms, re-exports, and doc comments do not
  count). Every file has at most one `#[cfg(test)]`, followed by `mod tests {` or
  `pub(crate) mod testutil {`, so the cut cannot drop production code.
* `tests/`, `bench/`, and `#[cfg(test)]` code are excluded from "production".

**Current scope.** External context, compaction summaries/epochs, checkpoint barriers,
WAL recovery, and redaction-aware session projections are included. The source and focused
fixtures remain the final authority for fields and producer call sites.

## 1. Events

Source of truth: `crates/rupi-core/src/event.rs`.

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
  (`crates/rupi-store/src/journal.rs:105`), written into the envelope at
  `journal.rs:113` (`entry.envelope.meta.seq = Some(seq);`) and remembered at
  `journal.rs:125`. Sequence numbers therefore start at `1`.
* `Store::emit` (`crates/rupi-store/src/store.rs:338`) returns that journal sequence and
  stamps it back into the caller's envelope, which is why `append_message` rejects an
  envelope with `seq == None` (`store.rs:387-393`, `let seq = envelope.meta.seq
  .ok_or_else(|| {`): a session line may only point at a real journal position.
* `timestamp_ms` comes from `now_millis()` (`ids.rs:24`), the platform clock, called in
  `EventMeta::new` (`event.rs:76`). The clock is explicitly not assumed monotonic.
* `redactions` is added to the journal line by the store's redaction policy only when
  redactions occurred (`journal.rs:116-118`); it is a store-side field and is **not** part
  of `EventEnvelope`.

### 1.2 `AgentEvent` variants

Enumerated from `crates/rupi-core/src/event.rs`, including the unit variant
`ContextSummary`, the current `AgentEvent` has 24 variants. Wire tags are the
snake_case variant names (`event.rs`).

#### `session_started` — `AgentEvent::SessionStarted` (`event.rs:168`), payload `event.rs:284`

Purpose: the session exists and which model owns the first epoch.
Fields: `working_dir: String`, `model: ModelRef`, `capabilities: ModelCapabilities`,
`resumed: bool` (`#[serde(default)]`).
Producers: yes (`crates/rupi-runtime/src/turn.rs:520`)

#### `user_message` — `AgentEvent::UserMessage` (`event.rs:173`), payload `event.rs:294`

Purpose: a user message was accepted into canonical history.
Fields: `text: String`, `attachments: u32` (`#[serde(default)]`).
Producers: yes (`crates/rupi-runtime/src/turn.rs:408`)

#### `model_request_started` — `AgentEvent::ModelRequestStarted` (`event.rs:178`), payload `event.rs:303`

Purpose: a model request began, which is the boundary for partial output.
Fields: `epoch: u32`, `model: ModelRef`, `message_count: u32`,
`context_tokens_est: u64` (estimate, not a measurement), `tools_exposed: u32`.
Producers: yes (`crates/rupi-runtime/src/turn.rs:643`)

#### `reasoning_delta` — `AgentEvent::ReasoningDelta` (`event.rs:184`), payload `event.rs:314`

Purpose: reasoning-like text arrived, with its provenance claim.
Fields: `text: String`, `provenance: ReasoningProvenance`, `chunk_index: u32`.
Producers: yes (`crates/rupi-runtime/src/turn.rs:1329`)

#### `assistant_delta` — `AgentEvent::AssistantDelta` (`event.rs:190`), payload `event.rs:321`

Purpose: assistant prose arrived.
Fields: `text: String`, `chunk_index: u32`.
Producers: yes (`crates/rupi-runtime/src/turn.rs:1342`)

#### `model_request_completed` — `AgentEvent::ModelRequestCompleted` (`event.rs:195`), payload `event.rs:327`

Purpose: a model request finished and is attributable.
Fields: `epoch: u32`, `model: ModelRef`, `finish_reason: Option<String>`,
`input_tokens: Option<u64>` (logical prompt count, including cached tokens),
`uncached_input_tokens: Option<u64>`, `logical_prompt_tokens: Option<u64>`,
`cache_read_tokens: Option<u64>`, `cache_write_tokens: Option<u64>`,
`output_tokens: Option<u64>`, `provider_total_tokens: Option<u64>`, `duration_ms: u64`,
`tool_calls: u32`, `reasoning_provenance: Option<ReasoningProvenance>` (all `Option`
fields `#[serde(default, skip_serializing_if = "Option::is_none")]`).
Producers: yes (`crates/rupi-runtime/src/turn.rs:695`)

#### `model_retry` — `AgentEvent::ModelRetry` (`event.rs:200`), payload `event.rs:346`

Purpose: the same request is being retried against the same model.
Fields: `attempt: u32`, `max_attempts: u32`, `kind: ModelFailureKind`,
`retry_after_ms: Option<u64>`, `will_failover: bool` (`#[serde(default)]`).
Producers: yes (`crates/rupi-runtime/src/turn.rs:815`)

#### `model_failover` — `AgentEvent::ModelFailover` (`event.rs:205`), payload `event.rs:358`

Purpose: availability failure moved generation to the backup model.
Fields: `from: ModelRef`, `to: ModelRef`, `kind: ModelFailureKind`,
`gaps: Vec<CapabilityGap>` (`#[serde(default)]`), `compacted: bool` (`#[serde(default)]`).
Producers: yes (`crates/rupi-runtime/src/turn.rs:867`)

#### `model_epoch_started` — `AgentEvent::ModelEpochStarted` (`event.rs:211`), payload `event.rs:371`

Purpose: which model owned a span of generation, and what it was believed capable of at
that moment.
Fields: `epoch: u32`, `model: ModelRef`, `reason: EpochReason`,
`capabilities: ModelCapabilities`.
Producers: yes (`crates/rupi-runtime/src/turn.rs:530`)

#### `tool_requested` — `AgentEvent::ToolRequested` (`event.rs:216`), payload `event.rs:379`

Purpose: the model asked for a tool call and arguments are fully decoded.
Fields: `call_id: ToolCallId`, `name: String`, `arguments: serde_json::Value`,
`read_only: bool` (`#[serde(default)]`).
Producers: yes (`crates/rupi-runtime/src/turn.rs:1014`)

#### `tool_started` — `AgentEvent::ToolStarted` (`event.rs:221`), payload `event.rs:388`

Purpose: execution began, so a later crash has an observed boundary.
Fields: `call_id: ToolCallId`, `name: String`.
Producers: yes (`crates/rupi-runtime/src/turn.rs:1110`)

#### `tool_completed` — `AgentEvent::ToolCompleted` (`event.rs:226`), payload `event.rs:394`

Purpose: the call completed with a committed result.
Fields: `call_id: ToolCallId`, `name: String`, `state: ToolExecutionState` (*"Always
[`ToolExecutionState::Succeeded`]; kept explicit so that the journal states the claim
instead of implying it."*), `duration_ms: u64`, `status: Option<i64>`, `reduced: bool`
(`#[serde(default)]`), `blob: Option<BlobRef>`, `visible_bytes: u64`.
Producers: yes (`crates/rupi-runtime/src/turn.rs:1175`)

#### `tool_failed` — `AgentEvent::ToolFailed` (`event.rs:231`), payload `event.rs:414`

Purpose: the call completed with an observed failure.
Fields: `call_id: ToolCallId`, `name: String`, `message: String`, `duration_ms: u64`,
`status: Option<i64>`.
Producers: yes (`crates/rupi-runtime/src/turn.rs:1023`)

#### `tool_unknown` — `AgentEvent::ToolUnknown` (`event.rs:237`), payload `event.rs:424`

Purpose: completion could not be observed, which is not the same as failure.
Fields: `call_id: ToolCallId`, `name: String`, `why: String`, `mutating: bool`.
Producers: yes (`crates/rupi-runtime/src/turn.rs:1196`)

#### `external_context_retrieved` — `AgentEvent::ExternalContextRetrieved` (`event.rs:242`), payload `event.rs:435`

Purpose: external knowledge entered context, with citation and provenance.
Fields: `source: ExternalContextSource`, `citation: Option<String>`, `bytes: u64`,
`inline: bool`, and provider-owned `metadata: BTreeMap<String, String>`.
Producer: `TurnLoop::run_turn_with_external_context`
(`crates/rupi-runtime/src/turn.rs`) emits the event and records the associated
model-visible message; the session projection carries the typed
`SessionMessage.external_context` reference for resume. The TUI renders the
resource, citation, source metadata, byte count, and inline/reference state.

#### `context_reduced` — `AgentEvent::ContextReduced` (`event.rs:248`), payload `event.rs:445`

Purpose: model-visible payload was reduced to a bounded representation.
Fields: `reason: ReductionReason`, `original_bytes: u64`, `visible_bytes: u64`,
optional `removed_messages`/`retained_messages` history counts, optional `blob: BlobRef`,
optional `recovery_ref: String`, and optional `tool_call_id: ToolCallId`. Nonzero
`removed_messages` is also projected as a session `reduction` record.
Producers: yes (`crates/rupi-runtime/src/turn.rs:921`)

#### `context_compaction_started` — `AgentEvent::ContextCompactionStarted` (`event.rs:253`), payload `event.rs:458`

Purpose: compaction began at a safe boundary.
Fields: `level: ContextLevel`, `reason: String`.
Producer: `TurnLoop::compact_range`/phase paths. Consumed at
`crates/rupi-tui/src/transcript.rs:419`. See §1.4.

#### `context_compaction_completed` — `AgentEvent::ContextCompactionCompleted` (`event.rs:258`), payload `event.rs:464`

Purpose: compaction finished and what it retained.
Fields: `level: ContextLevel`, `removed_messages: u32`, `retained_messages: u32`,
`context_epoch: u32`. L1/L2 retained counts exclude a protected checkpoint
capsule; L3 checkpoint counts include that capsule.
Producer: `TurnLoop::compact_range`/checkpoint paths; consumed at
`crates/rupi-tui/src/transcript.rs:426`. See §1.4.

#### `context_summary` — `AgentEvent::ContextSummary` (`event.rs`)

Purpose: attributes the summary message that replaces a compacted range. The message is
canonical session content, while the compaction epoch records how it becomes model-visible.
Producer: runtime L1/L2 compaction paths.

#### `context_compaction_epoch` — `AgentEvent::ContextCompactionEpoch` (`event.rs`)

Purpose: records the context epoch, canonical sequence range replaced, summary identity,
and retained model-visible projection. Replay uses this marker without deleting the
underlying trace.
Producer: runtime L1/L2 compaction paths.

#### `checkpoint_created` — `AgentEvent::CheckpointCreated` (`event.rs:263`), payload `event.rs:472`

Purpose: an episode checkpoint capsule was written.
Fields: `checkpoint_id: CheckpointId`, `capsule_version: u32`,
`summarized_events: u64`, `path: String`, `context_epoch: u32` (optional for
legacy traces).
Producer: `TurnLoop::checkpoint`/`checkpoint_turn_prefix`. Consumed at
`crates/rupi-tui/src/transcript.rs:445`; the store
documents that the caller still owes the event
(`crates/rupi-store/src/store.rs:409`). See §1.4.

#### `turn_completed` — `AgentEvent::TurnCompleted` (`event.rs:268`), payload `event.rs:480`

Purpose: a turn ended and how.
Fields: `status: TurnStatus`, `duration_ms: u64`.
Producers: yes (`crates/rupi-runtime/src/turn.rs:601`)

#### `diagnostic` — `AgentEvent::Diagnostic` (`event.rs:275`), payload `event.rs:486`

Purpose: an operator-visible condition that is not a domain event.
Fields: `level: DiagnosticLevel`, `message: String`.
Producers: yes (`crates/rupi-runtime/src/turn.rs:582`)

#### `session_ended` — `AgentEvent::SessionEnded` (`event.rs:280`), payload `event.rs:492`

Purpose: the session ended.
Fields: `reason: SessionEndReason`.
Producers: yes (`crates/rupi-runtime/src/turn.rs:493`)

### 1.3 Supporting enums carried by payloads

All three are `#[serde(rename_all = "snake_case")]` and carry no tag.

* `SessionEndReason` (`event.rs:130`): `UserExit` (`:132`), `Restart` (`:134`),
  `Fatal { message: String }` (`:136`).
* `TurnStatus` (`event.rs:142`): `Completed` (`:143`), `Cancelled` (`:144`),
  `Failed { kind: ModelFailureKind }` (`:145`).
* `DiagnosticLevel` (`event.rs:151`): `Info` (`:152`), `Warn` (`:153`), `Error` (`:154`).

`AttributedMessage` (`event.rs:499`) is a struct, not an event variant:
`envelope: EventEnvelope`, `message: Message`, used by session reconstruction.

### 1.4 Compaction/checkpoint production and recovery

`context_compaction_started`, `context_summary`,
`context_compaction_epoch`, and `context_compaction_completed` are emitted by the
runtime's compaction path. `checkpoint_created` is emitted after its capsule is
written and carries the next context epoch. `StoreTrace` coordinates each event
that has a semantic projection with a per-session WAL; an incomplete L1/L2
lifecycle is refused rather than resumed with a partially reduced context. L3
checkpoint completion has no separate `context_compaction_started` event and is
recognized as a checkpoint lifecycle.

## 2. Session records

Source of truth: `crates/rupi-core/src/session.rs` (schema) and
`crates/rupi-store/src/session_log.rs` plus `crates/rupi-store/src/store.rs` (the write
path).

### 2.1 Spec correction: `session_record_kind()` is not found

`docs/archive/slices/SLICE_SCHEMAS.md:11-13` asks for "`session_record_kind()`'s mapping in
`src/session.rs`". That function does not exist, and neither does that path:

```console
$ grep -rn "fn session_record_kind" --include=*.rs .
(no output)
$ grep -rn "session_record_kind" --include=*.rs .
(no output)
$ git log --all --oneline -S"session_record_kind"
0b48ec4 docs: spec the schema reference slice
$ ls src/session.rs
ls: cannot access 'src/session.rs': No such file or directory
```

`session_record_kind` appears in exactly one commit in this repository's history — the
commit that wrote the slice spec — and in no file at any revision. The name was never
introduced, so it was never removed or renamed. It has **not** been recreated, renamed to
something else, or "regenerated" here. Root `src/` is the CLI binary (`cli.rs`,
`main.rs`, `run.rs`, `trace.rs`); the session schema lives at
`crates/rupi-core/src/session.rs`. §2.2 documents what the spec wanted — the discriminant
mapping — using the code that actually produces it.

### 2.2 `SessionRecord` and its discriminant mapping

`SessionRecord` is documented as one line of `sessions/<id>.jsonl`
(`crates/rupi-core/src/session.rs`) and declared in `session.rs`:

```rust
/// One line of the semantic session JSONL log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)] // session.rs
#[serde(rename_all = "snake_case", tag = "type")]              // session.rs:31
pub enum SessionRecord {                                        // session.rs:32
```

The mapping is performed by serde's internally-tagged representation, not by a hand-written
function: the `type` key holds the snake_case variant name (`session.rs:31`).

| Variant | Declared at | `type` value | Payload struct (definition) |
| --- | --- | --- | --- |
| `Header` | `session.rs:34` | `"header"` | `SessionHeader` (`session.rs:48`) |
| `Message` | `session.rs:36` | `"message"` | `SessionMessage` (`session.rs:69`) |
| `Epoch` | `session.rs:38` | `"epoch"` | `SessionEpochRecord` (`session.rs:87`) |
| `Compaction` | `session.rs:41` | `"compaction"` | `SessionCompactionRecord` (`session.rs:95`) |
| `CheckpointBarrier` | `session.rs:43` | `"checkpoint_barrier"` | `SessionCheckpointRecord` (`session.rs:105`) |
| `Reduction` | `session.rs:46` | `"reduction"` | `SessionReductionRecord` (`session.rs:128`) |

Enumerated with:

```sh
grep -n 'serde(rename_all = "snake_case", tag = "type")\|pub enum SessionRecord\
\|^  Header\|^  Message\|^  Epoch\|^  Compaction\|^  CheckpointBarrier' \
  crates/rupi-core/src/session.rs
```

The `"header"` / `"message"` / `"epoch"` / `"compaction"` / `"checkpoint_barrier"` strings
above are derived from the attribute at `session.rs:31`; **no fixture pins them.**
`grep -rn "checkpoint_barrier" . --exclude-dir=.git` returns only
`crates/rupi-core/src/session.rs:200`, which is the test function name
`checkpoint_barrier_carries_the_capsule`, not a wire string. The only test that pins tag
strings for an internally-tagged union is the event-side one at
`crates/rupi-runtime/src/turn.rs:1926-1934` (`"session_started"`,
`"model_epoch_started"`, `"user_message"`, `"model_request_started"`,
`"model_request_completed"`, `"turn_completed"`).

`SESSION_SCHEMA_VERSION: u32 = 3` (`session.rs`) is stamped into new headers.
Version 1 remains readable for files using only its original record variants;
version-1 files containing the version-2 `reduction` record are rejected rather
than partially read or silently shortened. Version 2 remains readable with a
zero/default checkpoint context epoch. A file claiming a newer version is also
refused (`crates/rupi-store/src/session_log.rs`).

### 2.3 Record payload fields, quoted from `session.rs`

`SessionHeader` (`session.rs:48`) — "Session metadata, written once and readable without
parsing the rest":
`session_id: SessionId`, `version: u32`, `started_at_ms: u64`, `working_dir: String`,
`model: ModelRef`, `parent_session: Option<SessionId>`,
`branched_from_event: Option<EventId>`, `imported_from: Option<String>`. The last three
carry `#[serde(default, skip_serializing_if = "Option::is_none")]`, and `imported_from` is
kept separate "so an import never pretends to be native" (`session.rs:61-62`).

`SessionMessage` (`session.rs:69`) — "A message with the attribution required for
multi-epoch sessions": `turn_id: TurnId`, `role: Role`, `message: Message`, `epoch: u32`,
`model: ModelRef`, `event_id: EventId`, `seq: Option<EventSeq>`, and optional
`external_context: ExternalContextRef`. `event_id` exists "so a session line can always be
traced back into the trace" (`session.rs:77-78`); `seq` is
`#[serde(default, skip_serializing_if = "Option::is_none")]` and is "Sequence number of that
event, when the log had assigned one" (`session.rs:80`). The external reference is
provider-neutral and retains citation/provenance plus source metadata for resume and
on-demand rehydration; old session lines decode it as absent.

`SessionEpochRecord` (`session.rs:87`): `epoch: u32`, `model: ModelRef`,
`reason: crate::capability::EpochReason`. Runtime epoch-start events are projected
into this record so resume can restore the active epoch and continue numbering.

`SessionCompactionRecord` (`session.rs:95`): `context_epoch: u32`,
`level: crate::context::ContextLevel`, `removed_messages: u32`, `retained_from: u32`,
`retained_messages: u32`, `summary_present: bool`, and optional canonical
`replaces_from`/`replaces_through` sequence bounds. For L1/L2 records,
`retained_messages` counts the semantic tail after the summary; the protected
checkpoint capsule is stored separately. L3 checkpoint records instead count the
capsule plus its retained tail. The latter fields let resume reconstruct
`[capsule?, summary, retained tail]` without confusing session-line and
canonical event-sequence coordinates; older records default them to zero/false.

`SessionReductionRecord` (`session.rs:128`) is the durable L0 history-eviction
projection: `event_id: EventId`, optional `seq: EventSeq`, `reason: ReductionReason`,
`removed_messages: u32`, and `retained_messages: u32`. `SessionLog::restore` applies
these records in order, draining exactly the evicted model-visible prefix rather
than re-expanding history after failover or resume.

`SessionCheckpointRecord` (`session.rs:105`): `checkpoint_id: CheckpointId`,
`capsule_version: u32`, `context_epoch: u32`, `capsule_path: String`,
`capsule: ContextCapsule`. The context epoch joins the barrier to its canonical
`CheckpointCreated` event and L3 completion. Modern `Store::restore` refuses an
incomplete barrier lifecycle rather than guessing the retained tail; direct
legacy session-log reads may still stage a barrier for compatibility. The
capsule is "duplicated here so that resume needs one read" (`session.rs:110-111`).

`SessionSummary` (`session.rs:120`) is not a journal line. It is the listing entry —
`session_id: SessionId`, `started_at_ms: u64`, `working_dir: String`, `model: ModelRef`,
`messages: u32`, `last_model: Option<ModelRef>`, `last_turn_preview: Option<String>`,
`closed: bool` — built "from headers plus a tail read, never from full hydration: session
metadata lookup is a startup-path concern" (`session.rs:117-118`). Produced by
`SessionLog::summary` (`crates/rupi-store/src/session_log.rs:164`) and
`SessionLog::summary_report` (`session_log.rs:172`), exposed through
`Store::summaries` (`crates/rupi-store/src/store.rs:217`).

### 2.4 The JSONL write path

Per-session durable state is named by the layout:
`sessions/<id>.jsonl` (semantic projection), `sessions/<id>.trace.jsonl`
(canonical high-resolution trace), `sessions/<id>.wal.jsonl` (short-lived
trace/projection recovery intents), and `leases/<id>.lease` (exclusive process
ownership, outside the deletable session directory). The WAL is compacted after clean commits; an uncommitted
intent blocks read-only continuation until `Store::resume` repairs or refuses
it. `begin` and `resume` hold the lease for the `Session` lifetime, while
retention takes the same lease nonblocking before deleting a victim.

* `SessionLog::create` (`session_log.rs:46`) / `create_with_policy` (`session_log.rs:51`)
  sanitize the header, serialize it with
  `serde_json::to_string(&SessionRecord::Header(header.clone()))?` (`session_log.rs:66`),
  open the writer (`:67`) and write the header as line one (`:68`).
* `SessionLog::append` (`session_log.rs:112`) is the only way a record is added. A second
  header is rejected: *"a session header is written exactly once, at creation"*
  (`session_log.rs:113-116`). Every record goes through `sanitize_record` before it is
  serialized (`session_log.rs:118`), which round-trips the record through
  `RedactionPolicy::apply_json` (`session_log.rs:216-223`), and is then written with the
  durable flag set: `self.writer.write_line(&line, true)?` (`session_log.rs:119`) —
  *"Session records are always durable."* (`session_log.rs:111`).
* `Store::record` (`store.rs:366`) is the thin public seam onto that append.
* `Store::append_message` (`store.rs:379`) builds a `SessionMessage` bound to an already
  emitted event and appends `SessionRecord::Message(record.clone())` (`store.rs:403`). Its
  doc states why: "Attribution is not decoration: without the epoch and model that produced
  a message, a session cannot say which model is responsible for a claim after a failover."
  (`store.rs:373-375`). The runtime's only production caller is
  `crates/rupi-runtime/src/store_trace.rs:52`.
* `Store::checkpoint` (`store.rs:415`) writes the capsule file atomically —
  `path.with_extension("json.tmp")` then `std::fs::rename` (`store.rs:424-426`) — and only
  then appends the barrier (`store.rs:434-436`).
* Producers, same rule as §1: `Header` yes (`crates/rupi-store/src/session_log.rs:66`),
  `Message` yes (`crates/rupi-store/src/store.rs:403`), `CheckpointBarrier` yes
  (`store.rs:436`); `Epoch`, `Compaction`, and `Reduction` are emitted by `StoreTrace`
  (`crates/rupi-runtime/src/store_trace.rs:48-108`) and recovered with their canonical
  joins. Legacy low-level callers may still append only the older record variants.

### 2.5 What the checkpoint barrier is for

The module states the model-visible contract directly (`session.rs:10-12`):

> [`SessionRecord::CheckpointBarrier`] marks everything before it as summarized by a
> capsule. Once the matching canonical checkpoint completion is present, model-visible
> reconstruction is `latest checkpoint + events after it`, which keeps the resumed context
> small. Strict `Store::restore` still scans canonical history to validate lifecycle
> integrity and unresolved side effects. A barrier without that completion is an interrupted
> lifecycle and is refused by `Store::restore`.

`restore` implements exactly that, under the doc comment "Restore session state as
`latest checkpoint + records after it`" (`session_log.rs:285-286`):

```rust
SessionRecord::CheckpointBarrier(barrier) => {   // session_log.rs:306
  // A later barrier supersedes an earlier one: everything before it is   // :307-308
  // already inside the newer capsule's scope.
  summarized_messages += messages.len();          // :309
  checkpoint = Some(barrier.capsule.clone());     // :310
  checkpoint_seq = last_seq;                      // :311
  messages.clear();                               // :312
}
```

Its result is `RestoredSession` (`session_log.rs:264`): `header: SessionHeader`,
`messages: Vec<SessionMessage>` ("When a checkpoint barrier exists this is the post-barrier
window only", `session_log.rs:266-267`), `checkpoint: Option<ContextCapsule>`,
`checkpoint_seq: Option<EventSeq>` (the coordinate used by projection/replay helpers),
`epochs: Vec<SessionEpochRecord>`, `compactions: Vec<SessionCompactionRecord>`,
`summarized_messages: usize`, `last_seq: Option<EventSeq>`,
`malformed_records: usize`, `total_records: usize`.

So the barrier is a model-context projection device, not a truncation: earlier lines stay in
the file and in canonical history, and `summarized_messages` exists "for honest UI reporting"
(`session_log.rs:277`). `Store::restore` reaches it through
`session_log::restore_from_report` after scanning canonical history for integrity and
lifecycle validation. The resumed model state does not re-expand the pre-barrier window.

## 3. Provenance

`ReasoningProvenance` — `crates/rupi-core/src/provenance.rs:25`, `#[serde(rename_all = "snake_case")]`
at line 24, `Copy + Eq + Hash`, `Serialize + Deserialize`. `as_str()` (line 38) is the stable machine
label used in trace, exports, and UI tags; `label()` (line 51) is the human label. The two are
deliberately different words for the same variant, and `label()`'s doc comment says why: *"intentionally
different from each other so that a reader cannot mistake inference for emitted reasoning."*

| Variant | `as_str()` | `label()` | Producers |
|---|---|---|---|
| `Native` | `native` | `reasoning` | yes — `crates/rupi-provider/src/decode.rs:100`, on a decoded reasoning delta |
| `ProviderSummary` | `provider_summary` | `provider summary` | yes — provider decode when `capabilities.exposed_reasoning` declares a provider summary |
| `Declared` | `declared` | `declared rationale` | yes — provider decode when the endpoint declares `declared` exposure |
| `Reconstructed` | `reconstructed` | `reconstructed rationale` | no automatic producer; reserved for evidence-scoped analysis and covered as a typed/rendered form |

`is_inferred()` (line 64) is true for `Reconstructed` **only**: `matches!(self, Self::Reconstructed)`. Its doc
comment is explicit that *"Declared counts as authored output, not as inference"* — the runtime asked for
it, so a reader holds the model accountable for it, even though nobody watched it being produced.

**Why the reserved row stays.** The renderer, style roles, and stored field handle all
four variants, but `Reconstructed` is intentionally not produced by ordinary runtime or
import paths. Keeping it typed prevents an analysis result from being mistaken for model
emission; the release does not claim hidden chain-of-thought recovery.

The rule this table exists to enforce, quoted rather than paraphrased (`AGENTS.md:30`–`31`):

> 4. Never conflate native reasoning, provider summaries, declared rationale, and reconstructed rationale.
> 5. Never claim hidden chain-of-thought was recovered unless it was actually exposed.
