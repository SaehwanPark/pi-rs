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

## 2. Session records

Source of truth: `crates/pi-rs-core/src/session.rs` (schema) and
`crates/pi-rs-store/src/session_log.rs` plus `crates/pi-rs-store/src/store.rs` (the write
path).

### 2.1 Spec correction: `session_record_kind()` is not found

`docs/SLICE_SCHEMAS.md:11-13` asks for "`session_record_kind()`'s mapping in
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
`crates/pi-rs-core/src/session.rs`. §2.2 documents what the spec wanted — the discriminant
mapping — using the code that actually produces it.

### 2.2 `SessionRecord` and its discriminant mapping

`SessionRecord` is documented as *"One line of `session.jsonl`."*
(`crates/pi-rs-core/src/session.rs:29`) and declared at `session.rs:32`:

```rust
/// One line of `session.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)] // session.rs:30
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

Enumerated with:

```sh
grep -n 'serde(rename_all = "snake_case", tag = "type")\|pub enum SessionRecord\
\|^  Header\|^  Message\|^  Epoch\|^  Compaction\|^  CheckpointBarrier' \
  crates/pi-rs-core/src/session.rs
```

The `"header"` / `"message"` / `"epoch"` / `"compaction"` / `"checkpoint_barrier"` strings
above are derived from the attribute at `session.rs:31`; **no fixture pins them.**
`grep -rn "checkpoint_barrier" . --exclude-dir=.git` returns only
`crates/pi-rs-core/src/session.rs:200`, which is the test function name
`checkpoint_barrier_carries_the_capsule`, not a wire string. The only test that pins tag
strings for an internally-tagged union is the event-side one at
`crates/pi-rs-runtime/src/turn.rs:1926-1934` (`"session_started"`,
`"model_epoch_started"`, `"user_message"`, `"model_request_started"`,
`"model_request_completed"`, `"turn_completed"`).

`SESSION_SCHEMA_VERSION: u32 = 1` (`session.rs:27`) is stamped into the header's `version`
field, and a file claiming a newer version is refused rather than partially read
(`crates/pi-rs-store/src/session_log.rs:226-231`).

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
`model: ModelRef`, `event_id: EventId`, `seq: Option<EventSeq>`. `event_id` exists "so a
session line can always be traced back into the trace" (`session.rs:77-78`); `seq` is
`#[serde(default, skip_serializing_if = "Option::is_none")]` and is "Sequence number of that
event, when the log had assigned one" (`session.rs:80`).

`SessionEpochRecord` (`session.rs:87`): `epoch: u32`, `model: ModelRef`,
`reason: crate::capability::EpochReason`.

`SessionCompactionRecord` (`session.rs:95`): `context_epoch: u32`,
`level: crate::context::ContextLevel`, `removed_messages: u32`, `retained_from: u32`
("Line index (0-based, header excluded) of the first retained message.", `session.rs:99`).

`SessionCheckpointRecord` (`session.rs:105`): `checkpoint_id: CheckpointId`,
`capsule_version: u32`, `capsule_path: String`, `capsule: ContextCapsule`. The capsule is
"duplicated here so that resume needs one read" (`session.rs:110-111`).

`SessionSummary` (`session.rs:120`) is not a journal line. It is the listing entry —
`session_id: SessionId`, `started_at_ms: u64`, `working_dir: String`, `model: ModelRef`,
`messages: u32`, `last_model: Option<ModelRef>`, `last_turn_preview: Option<String>`,
`closed: bool` — built "from headers plus a tail read, never from full hydration: session
metadata lookup is a startup-path concern" (`session.rs:117-118`). Produced by
`SessionLog::summary` (`crates/pi-rs-store/src/session_log.rs:164`) and
`SessionLog::summary_report` (`session_log.rs:172`), exposed through
`Store::summaries` (`crates/pi-rs-store/src/store.rs:217`).

### 2.4 The JSONL write path

Two files per session, named by the layout:
`sessions/<id>.jsonl` (`crates/pi-rs-store/src/layout.rs:75-80`,
`const SESSION_EXTENSION: &str = "jsonl"` at `layout.rs:39`) and
`sessions/<id>.trace.jsonl` (`layout.rs:82-84`,
`const TRACE_SUFFIX: &str = ".trace.jsonl"` at `layout.rs:40`). The module header calls the
first "semantic session state" and the second "high-resolution trace"
(`layout.rs:9-10`).

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
  `crates/pi-rs-runtime/src/store_trace.rs:52`.
* `Store::checkpoint` (`store.rs:415`) writes the capsule file atomically —
  `path.with_extension("json.tmp")` then `std::fs::rename` (`store.rs:424-426`) — and only
  then appends the barrier (`store.rs:434-436`).
* Producers, same rule as §1: `Header` yes (`crates/pi-rs-store/src/session_log.rs:66`),
  `Message` yes (`crates/pi-rs-store/src/store.rs:403`), `CheckpointBarrier` yes
  (`store.rs:436`); `Epoch` **none found** and `Compaction` **none found**. Both are read —
  `restore` accumulates them (`session_log.rs:304-305`) and `summary_report` reads `Epoch`
  (`session_log.rs:191`) — but no production code appends them. As with §1.4 this is
  recorded, not fixed.

### 2.5 What the checkpoint barrier is for

The module states the contract directly (`session.rs:10-12`):

> [`SessionRecord::CheckpointBarrier`] marks everything before it as summarized by a
> capsule. Resume cost is then `latest checkpoint + events after it`, which is what keeps
> large historical sessions cheap to open.

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
`checkpoint_seq: Option<EventSeq>` ("so that post-checkpoint trace events can be read
without a full scan", `session_log.rs:272-273`; the read side is
`Journal::read_after(path, seq)` at `crates/pi-rs-store/src/journal.rs:165`),
`epochs: Vec<SessionEpochRecord>`, `compactions: Vec<SessionCompactionRecord>`,
`summarized_messages: usize`, `last_seq: Option<EventSeq>`,
`malformed_records: usize`, `total_records: usize`.

So the barrier is a resume-cost device, not a truncation: earlier lines stay in the file and
in canonical history, and `summarized_messages` exists "for honest UI reporting"
(`session_log.rs:277`). `Store::restore` reaches it through
`session_log::restore(&self.layout.session_path(session))` (`store.rs:192`). Session state
never depends on reading `*.trace.jsonl`.
