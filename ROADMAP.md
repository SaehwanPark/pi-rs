# ROADMAP

## Roadmap conventions

- `[ ]` not started
- `[-]` in progress
- `[x]` complete
- `[!]` blocked or requires architectural decision

Each stage has a **stage gate**. Do not advance merely because some tasks are complete; the gate must be satisfied.

---

## Phase 0 — Repository and architecture foundation

### Project setup

- [x] Create Cargo workspace.
- [x] Add pinned Rust toolchain.
- [x] Add formatting, clippy, test, and documentation CI.
- [x] Add `README.md`.
- [x] Add `ARCHITECTURE.md`.
- [x] Add `COMPATIBILITY.md`.
- [x] Add `ROADMAP.md`.
- [x] Add `AGENTS.md`.
- [x] Add canonical project design under `docs/`.
- [ ] Define contribution and issue templates.
- [x] Add benchmark harness directory.

### Core contracts

- [x] Define provider trait.
- [x] Define model capability schema.
- [x] Define typed provider failure classes.
- [x] Define `AgentEvent`.
- [x] Define event identity/order metadata.
- [x] Define session ID / turn ID / tool-call ID types.
- [x] Define tool lifecycle state.
- [x] Define reasoning provenance enum.
- [x] Define model epoch schema.
- [x] Define initial session storage schema.
- [x] Define initial trace storage schema.
- [x] Define redaction boundary.
- [x] Define project-trust boundary.

### Performance baseline

- [ ] Add cold-start benchmark.
- [ ] Add warm-start benchmark.
- [ ] Add TUI render benchmark.
- [ ] Add slash-completion benchmark.
- [ ] Record initial latency budgets.

### Stage gate

- [ ] Core contracts compile independently of provider/TUI implementation.
- [ ] Event/session/provenance schemas are documented.
- [ ] CI is green on supported platforms.
- [ ] Startup benchmark can run reproducibly.

---

## Phase 1 — Minimal usable coding agent

### CLI and TUI

- [x] Implement executable entry point (`pi-rs run` one-shot headless turn).
- [ ] Implement minimal terminal editor.
- [x] Implement streamed assistant rendering for the one-shot command.
- [ ] Implement cancel/interrupt.
- [ ] Implement compact status line.
- [x] Render the streamed transcript through a semantic layer (`pi-rs-tui`: roles,
      provenance-labelled reasoning, spelled-out tool state, calm-by-default diagnostics)
      and wire it into `pi-rs run` behind `--color/--no-color`, `--width`, `--no-reasoning`,
      `--verbose`, `--quiet`, `--silent`.
- [ ] Implement syntax-aware command parsing (parser exists in `pi-rs-tui::command`,
      quote-aware and tested, but no interactive editor consumes it yet).
- [ ] Implement syntax highlighting for operation vs arguments.
- [x] Implement path-aware rendering (`pi-rs-tui::command` path shapes, `Role::Path`).
- [x] Ensure narrow-terminal fallback (`MIN_COLUMN` drops decoration, keeps word alignment).
- [ ] Measure keystroke/render latency.

### Providers

> `pi-rs-provider` implements one OpenAI-compatible adapter (`OpenAiCompat`) used
> against local llama.cpp. Streaming, tool-call, exposed-reasoning, and failure
> normalization are covered by unit tests plus wire-level integration tests
> against a fake OpenAI server (`crates/pi-rs-provider/tests/transport.rs`), and
> verified against the real local endpoint: reasoning arrived as a separate typed
> event with `Native` provenance, the visible answer stayed separate, and the
> turn reported `finish_reason=stop` with usage. "One remote/cloud-compatible
> provider" stays open until verified against a real remote endpoint; credential
> handling (`api_key`, `api_key_env`, redaction) is implemented and tested.

- [x] Implement one local/OpenAI-compatible provider.
- [ ] Implement one remote/cloud-compatible provider.
- [x] Normalize streaming output.
- [x] Normalize tool-call output.
- [x] Normalize exposed reasoning.
- [x] Normalize provider failures.

### Basic tools

> `pi-rs-tools` provides the built-in set (`read`, `write`, `edit`, `grep`,
> `exec`) behind a `ToolRegistry`. Every path is confined to an explicit
> workspace root; every result passes one reduction boundary; every call lands in
> a typed lifecycle state. Two invariants carry the safety weight and are tested
> directly: a mutating tool that claims success while cancellation was observed
> is coerced to `Unknown`, and an approval question that nobody answers is a
> refusal rather than a permission.

- [x] Implement file read.
- [x] Implement file write/edit.
- [x] Implement shell command execution.
- [x] Implement grep/search.
- [x] Emit tool lifecycle events.
- [x] Mark read-only vs mutating tools.
- [x] Confine tools to an explicit workspace root.
- [x] Bound tool output before it reaches context.
- [x] Gate mutating tools behind policy and approval.

### Sessions

> The one-shot `pi-rs run --config <file> --cwd <workspace> --prompt <text>`
> composition root now drives the provider/tool loop and writes attributed
> messages plus the store-sequenced canonical trace. Interactive resume UX
> remains outside this slice.

- [x] Persist user/assistant/tool messages.
- [x] Resume latest session.
- [x] Create new session.
- [x] Preserve model identity per turn.
- [x] Avoid deep trace loading during startup.

### Stage gate

- [ ] User can start `pi-rs`, issue a coding request, inspect files, edit files, run tests, and continue the session.
- [ ] Startup is within an acceptable baseline.
- [ ] Optional integrations are not required for basic use.
- [ ] All tool actions produce durable lifecycle events.

---

## Phase 2 — Trace and provenance

### Event store

- [x] Persist typed event stream.
- [x] Guarantee stable event ordering.
- [x] Add model request start/end events.
- [x] Add native reasoning events.
- [x] Add tool request/start/completion events.
- [x] Add provider failure/retry events.
- [x] Add model epoch events.

### Trace storage

- [x] Add `trace.jsonl`.
- [x] Add blob storage for large payloads.
- [x] Add content hashing for stored payloads.
- [ ] Add optional compression.
- [x] Add trace retention configuration.
- [x] Keep raw provider payload capture disabled by default.

### Provenance

- [x] Render native reasoning distinctly.
- [x] Render provider summaries distinctly.
- [ ] Add declared-rationale representation.
- [ ] Add reconstructed-rationale representation.
- [ ] Prevent provenance loss during serialization.

> `Declared` and `Reconstructed` already have distinct roles and labels in the
> rendering layer, and a folded reasoning run never merges across a provenance
> boundary. These two stay open until a runtime path actually produces them.

### Inspection

- [x] Implement `pi-rs trace`.
- [x] Implement trace filtering by tools.
- [x] Implement trace filtering by reasoning.
- [x] Implement model-epoch inspection.

### Stage gate

> Model attribution is now per event: the envelope names the model in charge when the
> event was written, and a transition event keeps the epoch it *describes* separate from
> the epoch that recorded it. `tests/failover_cli.rs` reads the journal back and checks
> both. The reasoning-provenance lines below stay open until serialization is covered.

- [x] A completed session can answer which model produced each major event.
- [ ] Native reasoning remains distinguishable from all inferred/summarized forms.
- [ ] Large payloads do not require full inline duplication in trace JSONL.

---

## Phase 3 — Pi compatibility foundation

### Skills and prompts

- [ ] Implement Pi-style skill discovery.
- [ ] Implement `SKILL.md` loading.
- [ ] Implement prompt-template discovery.
- [ ] Add package-local skill support.
- [ ] Add project-local skill support.
- [ ] Add compatibility fixtures.

### Packages

- [ ] Parse compatible package manifests.
- [ ] Implement package discovery.
- [ ] Implement package install path.
- [ ] Report unsupported package surfaces.
- [ ] Add `pi-rs compat` prototype.

### Sessions

- [x] Implement Pi session import prototype.
- [x] Carry imported conversation messages into the session message log.
- [ ] Import a whole Pi session directory, or name what a single-file import leaves out.
- [ ] Implement Pi session export prototype.
- [ ] Document non-round-trippable metadata.

### Stage gate

- [ ] Representative Pi skills run unchanged.
- [ ] Prompt templates are reusable.
- [ ] Package compatibility diagnostics are useful and explicit.
- [ ] Compatibility tests run in CI.

---

## Phase 4 — Context lifecycle

### Context state

- [ ] Define `ContextState`.
- [ ] Define `ContextPolicy`.
- [ ] Define context-action enum.
- [ ] Track token estimates/measurements.
- [ ] Track recent-context target.
- [ ] Track context compaction epochs.

### Profiles

- [ ] Implement `balanced`.
- [ ] Implement `aggressive`.
- [ ] Implement `relaxed`.
- [ ] Lower thresholds for constrained windows.
- [ ] Do not auto-scale upward for large advertised windows.
- [ ] Expose advanced numeric overrides.

### L0 reduction

- [ ] Detect oversized new tool output.
- [ ] Archive full output.
- [ ] Replace with bounded model-visible representation.
- [ ] Preserve recovery reference.
- [ ] Log reduction event.

### L1 ordinary compaction

- [ ] Implement ordinary compaction.
- [ ] Retain recent context.
- [ ] Persist compaction event.
- [ ] Preserve original trace.

### L2 semantic phase compaction

- [ ] Implement phase-boundary request.
- [ ] Add `/compact-phase`.
- [ ] Allow model-facing semantic compaction recommendation.
- [ ] Restrict execution to safe idle boundaries.
- [ ] Add cooldown/rearm gate.

### L3 checkpoint/reset

- [ ] Define structured capsule schema.
- [ ] Implement checkpoint archive.
- [ ] Implement reviewed reset workflow.
- [ ] Add `/checkpoints`.
- [ ] Preserve unresolved constraints and next actions.
- [ ] Keep reset user-reviewable.

### Session resume optimization

- [ ] Resume from latest checkpoint + post-checkpoint events.
- [ ] Avoid full historical trace hydration.
- [ ] Benchmark large-session restore.

### Stage gate

- [ ] Long sessions can compact without losing canonical trace.
- [ ] Context profile requires no manual numeric tuning for normal use.
- [ ] Checkpoint/reset is reviewable and recoverable.
- [ ] Resume time remains low for large historical sessions.

---

## Phase 5 — Model failover

### Failure classification

- [ ] Implement retryable transport failures.
- [ ] Implement timeout classification.
- [ ] Implement rate-limit classification.
- [x] Implement provider-unavailable classification.
- [ ] Implement authentication classification.
- [ ] Implement protocol-failure classification.
- [ ] Implement context-overflow classification.
- [ ] Separate semantic/quality failures from availability failures.

### Retry

- [x] Add bounded retry policy.
- [ ] Add backoff.
- [x] Emit retry events.
- [ ] Support cancellation during retry.

### Backup model

- [x] Add primary/backup configuration.
- [x] Validate backup config without eagerly initializing it.
- [ ] Add manual `/failover`.
- [x] Add model epoch transition.

### Capability gate

- [ ] Compare primary and backup capabilities.
- [ ] Detect missing image support.
- [ ] Detect missing tool support.
- [ ] Detect smaller context window.
- [ ] Rebudget/compact before takeover when possible.
- [ ] Refuse impossible failover explicitly.

### Side-effect continuity

- [x] Preserve committed tool results across failover.
- [ ] Detect `Unknown` tool completion.
- [ ] Prevent blind replay of mutating operations.
- [ ] Add reconciliation path for uncertain state.

### Recovery policy

- [ ] Keep backup active after failover.
- [ ] Add explicit switch-back command.
- [x] Avoid automatic ping-pong.

### Stage gate

> The checked lines below are evidenced by `tests/failover_cli.rs`, which drives the
> real CLI against loopback HTTP endpoints: a 503 twice, then a backup. The capability
> gate and the smaller-backup path stay open because no test yet compares two models
> with different capabilities.

- [x] Simulated provider failure can continue on backup without replaying committed side effects.
- [ ] Smaller backup context is handled through compaction or explicit refusal.
- [x] Failover provenance is visible in trace and UI.
- [x] Backup initialization does not slow normal startup.

---

## Phase 6 — MCP client

### Protocol

- [ ] Implement MCP transport abstraction.
- [ ] Implement stdio transport.
- [ ] Implement supported network transport.
- [ ] Add protocol negotiation.
- [ ] Add selected older-version compatibility.
- [ ] Normalize tool schemas.

### Lazy discovery

- [ ] Do not connect all configured servers at startup.
- [ ] Add server activation API.
- [ ] Add capability filtering.
- [ ] Add lazy schema discovery.
- [ ] Cache discovered capabilities.
- [ ] Measure first-use latency.

### Tool integration

- [ ] Normalize MCP tools into internal tool abstraction.
- [ ] Preserve provenance/source metadata.
- [ ] Emit MCP tool lifecycle events.

### Stage gate

- [ ] A configured MCP server can be used without delaying startup.
- [ ] Large MCP catalogs do not all enter model context by default.
- [ ] MCP tools participate in the same trace/tool lifecycle as native tools.

---

## Phase 7 — `rkb-rs` first-party integration

### Integration package

- [ ] Create `pi-rs-rkb` package/extension.
- [ ] Add setup/discovery.
- [ ] Add RKB skill.
- [ ] Add MCP connection path.
- [ ] Add citation-aware rendering.

### External-context model

- [ ] Implement `ExternalContextRef`.
- [ ] Preserve durable RKB resource IDs.
- [ ] Preserve source/citation metadata.
- [ ] Allow compaction from inline evidence to reference.
- [ ] Allow rehydration on demand.
- [ ] Emit external-context retrieval events.

### Stage gate

- [ ] RKB evidence can enter context, be compacted to references, and later rehydrate.
- [ ] Source provenance survives all context transformations.
- [ ] RKB remains an independent project with no core dependency.

---

## Phase 8 — TypeScript extension compatibility host

### Host runtime

- [ ] Define host RPC protocol.
- [ ] Spawn Node host lazily.
- [ ] Implement extension process lifecycle.
- [ ] Isolate extension failures from core process.

### Extension APIs

- [ ] Support tool registration.
- [ ] Support slash-command registration.
- [ ] Support selected lifecycle events.
- [ ] Support context hooks.
- [ ] Support selected TUI hooks.
- [ ] Add real-world compatibility fixtures.

### Stage gate

- [ ] Representative Pi TypeScript extensions run with minimal/no changes.
- [ ] Node is not launched when no compatible extension requires it.
- [ ] Extension failure does not corrupt the core session.

---

## Phase 9 — MCP server / worker mode

### Agent API

- [ ] Implement `agent.start`.
- [ ] Implement `agent.continue`.
- [ ] Implement `agent.cancel`.
- [ ] Implement `agent.branch`.
- [ ] Implement `agent.compact`.

### Resources

- [ ] Expose session state.
- [ ] Expose summary.
- [ ] Expose messages.
- [ ] Expose trace.
- [ ] Expose diff.
- [ ] Expose artifacts.
- [ ] Expose latest checkpoint.

### Orchestration semantics

- [ ] Support long-running execution semantics.
- [ ] Keep internal trace separate from external summaries.
- [ ] Preserve model epochs and failover history.
- [ ] Keep API coarse and stable.

### Stage gate

- [ ] External orchestrator can run and inspect a `pi-rs` worker without scraping terminal output.
- [ ] Worker API does not expose unnecessary internal implementation details.

---

## Phase 10 — Replay and research tooling

### Replay

- [ ] Implement replay command.
- [ ] Filter by tools.
- [ ] Filter by reasoning.
- [ ] Filter by timing.
- [ ] Replay until event.
- [ ] Reconstruct model-visible context at an event.

### Branching

- [ ] Branch from historical event.
- [ ] Mark new execution separately from history.
- [ ] Compare two continuations from same state.

### Analysis

- [ ] Add model-epoch timeline.
- [ ] Add compaction timeline.
- [ ] Add failover timeline.
- [ ] Add provenance summary.
- [ ] Add trace export.

### Stage gate

- [ ] Historical execution can be inspected deterministically without confusing replay with new generation.
- [ ] Model-visible context can be reconstructed for selected events.

---

## Phase 11 — Adaptive optimization experiments

Do not begin until stable baselines exist.

### Context adaptation

- [ ] Measure prefill latency vs context size.
- [ ] Detect model/runtime-specific performance knees.
- [ ] Prototype adaptive context thresholds.
- [ ] Compare against static profiles.
- [ ] Keep adaptive mode opt-in initially.

### Backup optimization

- [ ] Evaluate optional warm standby.
- [ ] Measure startup/memory trade-offs.
- [ ] Keep cold backup as default.

### MCP optimization

- [ ] Explore predictive/lazy capability prefetch.
- [ ] Measure schema exposure vs model performance.
- [ ] Keep minimal exposure as default.

### Stage gate

- [ ] Experimental optimization demonstrates measurable benefit without degrading predictability.
- [ ] Static/default behavior remains available and stable.

---

## Ongoing cross-cutting work

### Performance

- [ ] Track cold startup regression.
- [ ] Track warm startup regression.
- [ ] Track TUI render latency.
- [ ] Track resume latency.
- [ ] Track Node host activation latency.
- [ ] Track MCP first-use latency.
- [ ] Keep startup-path dependency review active.

### Security

- [ ] Maintain redaction tests.
- [ ] Audit file permissions.
- [ ] Audit raw payload opt-in.
- [ ] Audit project-trust handling.
- [ ] Audit extension/MCP subprocess boundaries.

### Compatibility

- [ ] Track upstream Pi changes.
- [ ] Update compatibility matrix.
- [ ] Add fixtures for popular public packages.
- [ ] Document intentional divergences.

### Documentation

- [ ] Keep canonical design current.
- [ ] Keep README concise.
- [ ] Keep architecture invariants current.
- [ ] Keep roadmap statuses current.
- [ ] Keep compatibility limitations explicit.
