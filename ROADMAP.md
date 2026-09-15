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
- [x] Define contribution and issue templates.
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

- [x] Add cold-start benchmark (`bench/cold_start.sh` and integrated `bench/startup.sh --cold` benchmark
      first-exec-of-a-fresh-inode vs execs #2-#3 across iterations; limitations and requirement for root
      `drop_caches` or fresh-VM boot harness documented per `docs/SLICE_COLD_START.md`).
- [x] Add warm-start benchmark (`bench/startup.sh` reports min/mean/median/max over N warm runs;
      `bench/warm_start.sh` benchmarks relaunching a process that continues a stored session).
- [x] Add TUI render benchmark (`bench/render.sh`; cases in
      `crates/pi-rs-tui/benches/render.rs`).
- [x] Add slash-completion benchmark (`tab_completion` in `bench/keystroke.sh`: Tab cycling
      a command word against 64 candidates, budgeted per press alongside the other keystrokes).
- [x] Record initial latency budgets. They are the budgets in the bench source, which exits
      non-zero when a case exceeds one; the recorded baseline, its date, and the machine that
      produced it are in the same file, so the enforcement point and the number cannot drift
      apart.

### Stage gate

- [x] Core contracts compile independently of provider/TUI implementation (`pi-rs-core` has zero
      dependencies on provider, runtime, store, or TUI crates).
- [x] Event/session/provenance schemas are documented (`crates/pi-rs-core/src/`, `ARCHITECTURE.md`,
      and canonical design docs).
- [x] CI is green on supported platforms.
- [x] Startup benchmark can run reproducibly (`bench/startup.sh`).

---

## Phase 1 — Minimal usable coding agent

### CLI and TUI

- [x] Implement executable entry point (`pi-rs run` one-shot headless turn).
- [x] Implement minimal terminal editor, and the loop that feeds it (`pi-rs interactive`:
      raw mode, key events mapped through `pi-rs-tui::keys`, `pi-rs-tui::editor` buffer,
      one turn per submit on a single open session, so context carries across turns).
      Turn interruption is still its own item below.
- [x] Implement streamed assistant rendering for the one-shot command.
- [x] Implement cancel/interrupt (`interrupt::TurnInterruptGuard` catches `SIGINT` during in-flight
      turns and flags `CancelToken`, safely aborting model streaming or tool calls without
      session corruption).
- [x] Implement compact status line (the projection in `pi-rs-tui::statusline` is rendered
      by the interactive session frame, tested across narrow fallback and idle/working states).
- [x] Render the streamed transcript through a semantic layer (`pi-rs-tui`: roles,
      provenance-labelled reasoning, spelled-out tool state, calm-by-default diagnostics)
      and wire it into `pi-rs run` behind `--color/--no-color`, `--width`, `--no-reasoning`,
      `--verbose`, `--quiet`, `--silent`.
- [x] Implement syntax-aware command parsing (parser in `pi-rs-tui::command` is consumed in
      interactive routing via `Input::parse` and prompt template argument parsing).
- [x] Implement syntax highlighting for operation vs arguments (`pi-rs-tui::highlight::tokens`
      wired into `interactive` buffer rows and painted across terminal palettes).
- [x] Implement path-aware rendering (`pi-rs-tui::command` path shapes, `Role::Path`).
- [x] Ensure narrow-terminal fallback (`MIN_COLUMN` drops decoration, keeps word alignment).
- [x] Measure keystroke/render latency. Keystroke latency is measured and budgeted in
      `bench/keystroke.sh` (`crates/pi-rs-tui/benches/keystroke.rs`: eleven cases); render and
      command-parse latency are measured and budgeted in `bench/render.sh` (`crates/pi-rs-tui/benches/render.rs`).

### Providers

> `pi-rs-provider` implements one OpenAI-compatible adapter (`OpenAiCompat`) used
> against local llama.cpp and remote cloud endpoints (OpenAI, Groq, OpenRouter).
> Streaming, tool-call, exposed-reasoning, and failure normalization are covered by
> unit tests plus wire-level integration tests against a fake OpenAI server
> (`crates/pi-rs-provider/tests/transport.rs`), and verified against the real local
> endpoint: reasoning arrived as a separate typed event with `Native` provenance,
> the visible answer stayed separate, and the turn reported `finish_reason=stop` with
> usage. Remote cloud provider paths (`ModelEndpoint::remote`, `ProviderConfig::remote`,
> `OpenAiCompat::remote`) support Bearer authorization via environment variables
> (`api_key_env`), custom headers (e.g. `HTTP-Referer`, `X-Title`), default base URL
> fallback (`DEFAULT_OPENAI_BASE_URL`), and cloud error classification.

- [x] Implement one local/OpenAI-compatible provider.
- [x] Implement one remote/cloud-compatible provider.
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
- [x] Resume a recorded session by id or unique prefix.
- [x] Create new session.
- [x] Preserve model identity per turn.
- [x] Avoid deep trace loading during startup.

### Stage gate

- [x] User can start `pi-rs`, issue a coding request, inspect files, edit files, run
      tests, and continue the session (`tests/run_cli.rs`, `tests/resume_cli.rs`, and
      `src/interactive.rs`).
- [x] Startup is within an acceptable baseline (latest recorded run: cold 222.06 ms,
      warm median 3.02 ms; see
      `docs/MVP_VALIDATION.md`; aspirational
      targets remain documented in README and the canonical design).
- [x] Optional integrations are not required for basic use (runs fully standalone without Node, MCP, or external tools).
- [x] All tool actions produce durable lifecycle events (`Requested`, `Started`, `Completed`, `Failed` recorded in `trace.jsonl` and session logs).

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
- [x] Add optional compression (opt-in raw Deflate for content-addressed blob payloads; `trace.jsonl` remains plain, references retain logical hash/size and encoding, and old raw references remain readable).
- [x] Add trace retention configuration.
- [x] Keep raw provider payload capture disabled by default.

### Provenance

- [x] Render native reasoning distinctly.
- [x] Render provider summaries distinctly.
- [x] Add declared-rationale representation.
- [x] Add reconstructed-rationale representation (typed `Reconstructed` provenance, `ReasoningChunk` serialization, and distinct TUI role/label; no producer is enabled until an explicit evidence-scoped analysis contract exists).
- [x] Prevent provenance loss during serialization.

> Provenance is decided by the endpoint's declared `exposed_reasoning` rather than by
> the response field it was decoded from, and `Declared` is therefore now produced:
> an endpoint declaring `declared` exposure yields declared rationale end to end.
> `crates/pi-rs-provider/tests/transport.rs` sends the same thinking field under three
> declarations and reads back three different claims; `tests/run_cli.rs` checks the
> claim through the journal and the rendered transcript line. A folded reasoning run
> still never merges across a provenance boundary.
>
> `Reconstructed` keeps a distinct role and label in the rendering layer and no
> producer: nothing in `pi-rs` infers reasoning after the fact, and it should not
> acquire that ability casually.
>
> Serialization is pinned instead of trusted. `tests/provenance_roundtrip.rs` walks all
> four claims through provider event, `trace.jsonl`, the session log, and the renderer,
> and asserts that a trace line which lost its claim is refused as malformed rather than
> read back as `Native`.

### Inspection

- [x] Implement `pi-rs trace`.
- [x] Implement trace filtering by tools.
- [x] Implement trace filtering by reasoning.
- [x] Implement model-epoch inspection.
- [x] Name the stored-out reference for an externalized field when rendering a recorded line. The transcript budgets a long argument value to keep one request on one line, which cuts the reference off; the marker is in the line and the typed `externalized` record carries the path. (Verified in `crates/pi-rs-tui/src/trace.rs` unit tests and `tests/trace_cli.rs`)

### Stage gate

> Model attribution is now per event: the envelope names the model in charge when the
> event was written, and a transition event keeps the epoch it *describes* separate from
> the epoch that recorded it. `tests/failover_cli.rs` reads the journal back and checks
> both. Provenance serialization is pinned rather than assumed, and the production
> path decides a claim from the endpoint's declaration, not from the wire field.

- [x] A completed session can answer which model produced each major event.
- [x] Native reasoning remains distinguishable from all inferred/summarized forms.
- [x] Large payloads do not require full inline duplication in trace JSONL.
  - Every journal line carries an inline budget (`WritePolicy::inline_threshold_bytes`,
    8 KiB by default). Above it, whole fields go to the session's blob store once —
    content addressing means two lines about the same payload share one copy — and
    the line keeps a bounded preview naming the reference and the original size,
    plus a typed `externalized` record so the sizes need no prose parsing.
  - Envelope bookkeeping and pointer-shaped fields are excluded on purpose: an
    elided identifier or `blob` record cannot be followed, so the alternative to a
    long line would be an unreadable one.
  - Bounding runs after redaction, never before, so bytes that leave the line are
    already the sanitized ones.

---

## Phase 3 — Pi compatibility foundation

### Skills and prompts

- [x] Implement Pi-style skill discovery.
- [x] Implement `SKILL.md` loading.
- [x] Implement prompt-template discovery.
- [x] Add package-local skill support.
- [x] Add project-local skill support.
- [x] Add compatibility fixtures.

Skill discovery (`pi-rs-compat::skill`, surfaced by `pi-rs skills`) reads the two file
families Pi documents — per-user and per-project — including the rule that a root `*.md`
counts as a skill in `.pi/` locations and is ignored in the shared `.agents/` ones, the
ancestor walk that stops at the git root, and the frontmatter subset (`name`,
`description`, `license`, `compatibility`, `allowed-tools`, `disable-model-invocation`)
with quoted scalars and `|`/`>` block scalars. Package-local skills from discovered
packages (global and project, gated on trust) and explicit `--skill` paths are supported.
Project locations are gated on trust, and frontmatter declares the trust question as an
input rather than answering it.

Prompt-template discovery (`pi-rs-compat::prompt`, surfaced by `pi-rs prompts`) reads
Pi's two template locations non-recursively, takes the command name from the filename, and
falls back to the body's first line for a missing `description` while recording that the
description was not authored. `pi-rs prompt <name> [args…]` applies Pi's substitution
grammar (`pi-rs-compat::substitute`) and prints the prompt alone, which is what makes a
template reusable before any session knows how to invoke one.

Open in this area, in order:

- [x] Load `SKILL.md` bodies and honour `disable-model-invocation` when activating.
- [x] Present loaded skills to the model as a skill-control prompt listing/template.
- [x] Add package-local skills (`manifest.skill_paths`, conventional `skills/` and `SKILL.md`),
      and `--skill` CLI paths. (Settings array remains deferred).
- [x] Invoke a prompt template from inside a session (`/name`), and wire `pi-rs run` to
      accept one; `pi-rs prompt` expands a template today, nothing sends it.
- [x] Split one typed string into template arguments the way Pi's editor does, quotes
      included.
- [x] Add package-local prompts (`manifest.prompt_paths`, conventional `prompts/`),
      `--prompt-template`, and `--no-prompt-templates`. (Settings array remains deferred).
- [x] Decide trust somewhere other than the file reader, then pass its answer in (`--project` is an explicit caller-owned one-shot grant; `--trust-store <dir>` resolves durable exact canonical project scopes, with denial winning over the one-shot flag; durable high-risk decisions are recorded by `pi-rs trust` in `FileTrustStore`, while runtime once/always prompting remains deferred).
- [x] Add the compatibility fixture suite (`tests/compat/` holds skill, prompt, and package
      fixtures; session import/export has targeted CLI coverage; TypeScript extension
      execution is covered by the Phase 8 host fixtures).

### Packages

- [x] Parse compatible package manifests (`crates/pi-rs-compat/src/package.rs`: hand-rolled
      zero-dependency JSON reader; extracts `name`, `version`, `description`, `pi.skills`,
      `pi.prompts`; produces typed `Warning::UnsupportedSurface` for `extensions`,
      `Warning::UnknownSurface` for unknown `pi`-namespace keys; `tests/compat_packages.rs`:
      9 fixture-driven integration tests pass; 25 unit tests pass).
- [x] Report unsupported package surfaces (per-surface `Warning` variants with counts;
      does not reject the whole package when only one optional feature is unrecognised;
      `extensions` is the documented Phase-8 surface, recorded with entry count).
- [x] Implement package discovery (`crates/pi-rs-compat/src/package.rs`: `discover(&Discovery)`
      scans `$HOME/.pi/agent/packages`, `$HOME/.pi/packages`, and `<ancestor>/.pi/packages`
      when trusted; deterministic lexicographical sort; first-found-wins duplicate resolution;
      subdirectories missing `package.json` flagged with `Warning::MissingManifest`;
      exposes contained `skill_locations` and `prompt_locations` via manifest or convention;
      `pi-rs packages [--project] [--trust-store <dir>] [--show <name>]` CLI command;
      `tests/compat_packages.rs`:
      9 passing fixture-driven tests; `tests/packages_cli.rs`: 6 passing CLI tests).
- [x] Implement package install path (explicit local-directory copy to global/project package roots with atomic staging, symlink/path checks, and no dependency/script execution; npm/git/HTTP/update support remains deferred and compatibility stays Partial).
- [x] Add `pi-rs compat` prototype (`crates/pi-rs-compat/src/compat.rs`: `inspect_target` inspects
      package manifests, skills, prompts, and extension files with static analysis of `registerTool`,
      `registerCommand`, context hooks, and internal imports; `pi-rs compat [options] <path-or-package>`
      with `--project` and `--json` support; `tests/compat_cli.rs`: 8 passing integration tests;
      `crates/pi-rs-compat/src/compat.rs`: 5 passing unit tests).

### Sessions

- [x] Implement Pi session import prototype.
- [x] Carry imported conversation messages into the session message log.
- [x] Import a whole Pi session directory: `import-pi <dir>` files every `*.jsonl` directly
      inside it, each as its own session; cross-file lineage is named, not reconstructed.
- [x] Implement Pi session export prototype.
- [x] Document non-round-trippable metadata. (Documented in `COMPATIBILITY.md` §10.3 and `docs/SESSION_COMPATIBILITY.md`; verified against import/export CLI loss reporting)

### Stage gate

> Skills, prompts, and package discovery gates are passed. The bounded local package-install path is covered; `--trust-store` resolves durable project grants/denials for compatibility discovery; remote source resolution, dependency installation, and automatic extension discovery remain deferred. The selected explicit TypeScript host surface is verified in Phase 8.
> `tests/compat_skills.rs` drives fixture skills (discovery, body loading, `disable-model-invocation`,
> `SKILL.md` frontmatter, project trust gate) — all 8 tests pass.
> `tests/compat_prompts.rs` drives fixture templates (Pi substitution grammar, argument splitting,
> description fallback, duplicate/malformed warnings) — all 9 tests pass.
> `tests/compat_packages.rs` drives fixture manifests and discovery (minimal, full, extensions, malformed, missing, home/project discovery)
> — all 9 fixture tests pass; 25 unit tests in `package.rs` pass;
> `tests/packages_cli.rs`: 6 CLI tests pass.
> Selected TypeScript extension compatibility is complete in Phase 8; full custom UI, dependency installation, and automatic project extension discovery remain deferred.

- [x] Representative Pi skills run unchanged (`tests/compat_skills.rs`: 8 passing fixture
      tests cover discovery, frontmatter, body loading, and project-trust gate).
- [x] Prompt templates are reusable (`tests/compat_prompts.rs`: 9 passing fixture tests cover
      Pi substitution grammar, argument splitting, and description fallback).
- [x] Package compatibility diagnostics are useful and explicit (`tests/compat_packages.rs`: 9 fixture-driven integration tests; `package.rs`: per-surface `Warning` variants; no whole-package rejection for a single unsupported surface).
- [x] Compatibility tests run in CI (`cargo test` includes `compat_skills`, `compat_prompts`, `compat_packages`, and `packages_cli` integration tests).

---

## Phase 4 — Context lifecycle

### Context state

- [x] Define `ContextState`.
- [x] Define `ContextPolicy`.
- [x] Define context-action enum.
- [x] Track token estimates/measurements.
- [x] Track recent-context target.
- [x] Track context compaction epochs.

### Profiles

- [x] Implement `balanced`.
- [x] Implement `aggressive`.
- [x] Implement `relaxed`.
- [x] Lower thresholds for constrained windows.
- [x] Do not auto-scale upward for large advertised windows.
- [x] Expose advanced numeric overrides.

### L0 reduction

> Evidenced by the oversized-tool-output path: `pi-rs-runtime` reads the journal back and
> asserts that a reduced result carries a `context_reduced` event whose recovery
> reference resolves to a blob holding the full, redacted output. A reduction that
> cannot be stored still logs the event, with no reference, rather than logging nothing.

- [x] Detect oversized new tool output.
- [x] Archive full output.
- [x] Replace with bounded model-visible representation.
- [x] Preserve recovery reference.
- [x] Log reduction event.

### L1 ordinary compaction

- [x] Implement ordinary compaction (`TurnLoop::compact` in `crates/pi-rs-runtime/src/turn.rs`).
- [x] Retain recent context (retained tail kept alongside canonical summary).
- [x] Persist compaction event (`ContextCompactionStarted`, `ContextSummary`, `ContextCompactionEpoch`, `ContextCompactionCompleted`).
- [x] Preserve original trace (original events remain in `trace.jsonl` with epoch tracking).
- [x] Recover once from an uncommitted provider `ContextOverflow` by compacting only
      pre-turn history and reissuing the exact normal request shape; committed output,
      no prehistory, a second refusal, or an unfit candidate remains terminal.

### L2 semantic phase compaction

- [x] Implement phase-boundary request (`TurnLoop::compact_phase` in `crates/pi-rs-runtime/src/turn.rs` creates structured phase summary epoch).
- [x] Add `/compact-phase` (interactive slash command with Tab completion and optional `--force` override).
- [x] Allow model-facing semantic compaction recommendation (`ContextAction::Compact` with `ContextLevel::L2Phase` routes to phase compaction).
- [x] Restrict execution to safe idle boundaries (phase compaction executed between turn requests or via interactive command when idle).
- [x] Add cooldown/rearm gate (5-second default cooldown interval between phase compactions, bypassable via `force: true` / `--force`).

### L3 checkpoint/reset

- [x] Define structured capsule schema (`ContextCapsule`, `CapsuleDecision`, `CapsuleArtifact`, `CAPSULE_SCHEMA_VERSION`).
- [x] Implement checkpoint archive (`Store::list_checkpoints`, `StoreTrace::create_checkpoint`, `sessions/<id>/checkpoints/<cp>.json`).
- [x] Implement reviewed reset workflow (`TurnLoop::checkpoint` resets visible messages to formatted capsule, advances context epoch).
- [x] Add `/checkpoints` (interactive command lists capsules with objectives, completion status, and artifacts; Tab completed).
- [x] Preserve unresolved constraints and next actions (`ContextCapsule` fields).
- [x] Keep reset user-reviewable (capsule contents viewable via `/checkpoints` and formatted structured block).

### Session resume optimization

- [x] Resume from latest checkpoint + post-checkpoint events (`continue_context` in `src/run.rs` prepends checkpoint capsule to restored messages).
- [x] Avoid full historical trace hydration (checkpoint acts as barrier for model context reconstruction).
- [x] Benchmark large-session restore (`bench/large_session.sh` and `crates/pi-rs-store/benches/restore.rs` measure 10, 100, 500, and 1,000 turns with and without checkpoint barriers).

### Stage gate

> Compaction-without-canonical-loss is evidenced by `tests/trace_cli.rs::a_compaction_epoch_record_drops_no_canonical_record`,
> which reads the trace journal after a compaction epoch and asserts every record remains addressable.
> Profile-based context requires no manual numeric tuning: `balanced`/`aggressive`/`relaxed` profiles work
> out of the box without per-session numeric overrides. Checkpoint is reviewable via `/checkpoints` command.
> Large-session resume benchmark is verified via `bench/large_session.sh` (1,000-turn historical restore runs in ~2.2 ms).

- [x] Long sessions can compact without losing canonical trace (`tests/trace_cli.rs::a_compaction_epoch_record_drops_no_canonical_record` asserts all records remain addressable after epoch).
- [x] Context profile requires no manual numeric tuning for normal use (`balanced`, `aggressive`, `relaxed` profiles work out of the box via `ContextPolicy` defaults).
- [x] Checkpoint/reset is reviewable and recoverable (`ContextCapsule` viewable via `/checkpoints`, checkpoint barrier recorded in trace with formatted structured block).
- [x] Resume time remains low for large historical sessions (`bench/large_session.sh` verifies 1,000 turns with checkpoint barriers restores in ~2.2 ms, bounding active message hydration to 200 messages).

---

## Phase 5 — Model failover

### Failure classification

- [x] Implement retryable transport failures (`ModelFailureKind::Transport` in `crates/pi-rs-core/src/failure.rs`).
- [x] Implement timeout classification (`ModelFailureKind::Timeout`, 408 / socket timeout).
- [x] Implement rate-limit classification (`ModelFailureKind::RateLimited`, 429 with retry-after header parsing).
- [x] Implement provider-unavailable classification (`ModelFailureKind::ProviderUnavailable`, 5xx, missing endpoint).
- [x] Implement authentication classification (`ModelFailureKind::Authentication`, 401/403).
- [x] Implement protocol-failure classification (`ModelFailureKind::Protocol`, malformed SSE/JSON/schema).
- [x] Implement context-overflow classification (`ModelFailureKind::ContextOverflow`, context window error parsing).
- [x] Separate semantic/quality failures from availability failures (`ModelFailureKind::Semantic` is non-availability and never retried or failed over).

### Retry

- [x] Add bounded retry policy.
- [x] Add backoff (exponential backoff with server retry-after honoring).
- [x] Emit retry events.
- [x] Support cancellation during retry (cancellation checked during backoff sleep).

### Backup model

- [x] Add primary/backup configuration.
- [x] Validate backup config without eagerly initializing it.
- [x] Add manual `/failover` (`SessionHandle::failover_manual`, interactive `/failover` command).
- [x] Add model epoch transition (`EpochReason::ManualSwitch`).

### Capability gate

> The gate compares declared capability snapshots, so it decides before any adapter is
> built. `FailoverPolicy::decide` is covered by `crates/pi-rs-runtime/src/failover.rs`
> tests for each gap kind, and the two decisions that reach a user — refusal by name,
> and a narrowed takeover that names the window it lost — are covered end to end in
> `tests/failover_cli.rs`.

- [x] Compare primary and backup capabilities.
- [x] Detect missing image support.
- [x] Detect missing tool support.
- [x] Detect smaller context window.
- [x] Rebudget/compact before takeover when possible.
- [x] Refuse impossible failover explicitly.

### Side-effect continuity

- [x] Preserve committed tool results across failover.
- [x] Detect `Unknown` tool completion (`ToolExecutionState::Unknown` and `AgentEvent::ToolUnknown`).
- [x] Prevent blind replay of mutating operations (cancelled/uncertain mutating calls coerced to `Unknown`).
- [x] Add reconciliation path for uncertain state (`ReconciliationStatus` on `Tool` trait, `ToolRegistry::reconcile`, and `TurnEngine::reconcile_tool_call` disambiguate `write`/`edit` side effects into `Committed`, `Unmodified`, `Diverged`, or `RequiresManualInspection`; verified in `crates/pi-rs-tools/tests/registry.rs`).

### Recovery policy

- [x] Keep backup active after failover (active model persists across subsequent turns).
- [x] Add explicit switch-back command (`SessionHandle::switch_back_manual`, interactive `/switch-back`).
- [x] Avoid automatic ping-pong.

### Stage gate

> The checked lines below are evidenced by `tests/failover_cli.rs`, which drives the
> real CLI against loopback HTTP endpoints: a 503 twice, then a backup whose endpoint
> declares different capabilities from the primary's. A text-only backup is refused by
> name and never contacted; a smaller-window backup takes over and the transcript names
> the window it lost.

- [x] Simulated provider failure can continue on backup without replaying committed side effects.
- [x] Smaller backup context is handled through compaction or explicit refusal.
- [x] Failover provenance is visible in trace and UI.
- [x] Backup initialization does not slow normal startup.

---

## Phase 6 — MCP client

### Protocol

- [x] Implement MCP transport abstraction (`McpTransport` trait).
- [x] Implement stdio transport (`StdioTransport` with child process and JSON-RPC 2.0).
- [x] Implement supported network transport (lazy Streamable HTTP POST with bounded JSON/SSE responses, session headers, status/id validation, and config redaction; long-lived push/cancellation remains deferred).
- [x] Add protocol negotiation (`initialize` handshake and version agreement).
- [x] Add selected older-version compatibility (`2024-11-05`, `2024-10-07`).
- [x] Normalize tool schemas (`McpToolDefinition` input schema mapped to tool JSON schema).

### Lazy discovery

- [x] Do not connect all configured servers at startup (startup verified at ~3.2 ms).
- [x] Add server activation API (`McpManager::enable_server`, `SessionHandle::mcp_enable`, `/mcp enable`).
- [x] Add capability filtering (`read_only_tools`, selective server activation).
- [x] Add lazy schema discovery (`tools/list` on activation).
- [x] Cache discovered capabilities (`McpManager::active_tools`).
- [x] Measure first-use latency (`first_use_latencies` reported in `/mcp`).

### Tool integration

- [x] Normalize MCP tools into internal tool abstraction (`McpTool` implementing `Tool`).
- [x] Preserve provenance/source metadata (`[MCP:{server}]` description, namespaced identifier).
- [x] Emit MCP tool lifecycle events (participates in `ToolRegistry` lifecycle, preserving `Unknown` on uncertain mutation).

### Stage gate

- [x] A configured MCP server can be used without delaying startup (warm startup ~3.2 ms vs <100 ms budget; HTTP construction performs no I/O).
- [x] Large MCP catalogs do not all enter model context by default (activation is lazy and filtered).
- [x] MCP tools participate in the same trace/tool lifecycle as native tools (verified via `mcp_integration.rs`).
- [x] A configured HTTP MCP endpoint is selected lazily and passes initialize/tools-list through the same client (`pi-rs-mcp` manager/transport wire tests); JSON and bounded SSE responses, session headers, notification 202/204, status/id failures, reserved headers, and response limits are covered.

---

## Phase 7 — `rkb-rs` first-party integration

### Integration package

- [x] Create `pi-rs-rkb` package/extension (`crates/pi-rs-rkb` is a downstream adapter;
      its bundled `package.json`/`SKILL.md` are inspectable without linking the external
      `rkb-rs` crate).
- [x] Add setup/discovery (`RkbSetup::discover` recognizes caller-provided `rkb`/`rkb-rs`
      MCP configurations without process or network I/O; `normalize_configs` preserves
      the endpoint while marking all known retrieval tools read-only before manager use).
- [x] Add RKB skill (bundled citation and rehydration instructions are offered only when
      an RKB server is configured, with explicit `/mcp enable` guidance for lazy activation).
- [x] Add MCP connection path (`RkbAdapter` activates the existing lazy manager only on
      explicit activation and normalizes read-only RKB retrieval tools).
- [x] Add citation-aware rendering (`RkbContextEntry` retains citation, URL, document,
      page, and record metadata; generic TUI rendering displays source metadata).

### External-context model

- [x] Implement `ExternalContextRef` (provider-neutral, serializable identity plus
      provider-owned metadata).
- [x] Preserve durable RKB resource IDs (`record_id` is retained in the reference and
      session projection).
- [x] Preserve source/citation metadata (retrieval events and `SessionMessage.external_context`
      retain citation, provenance, URL/document/page, and RKB record fields).
- [x] Allow compaction from inline evidence to reference (`compact_to_reference` and
      `RkbContext::compact_to_references` preserve identity without inline bytes).
- [x] Allow rehydration on demand (`RkbConnection::resolve_external_ref` performs an exact
      `search_chunks` lookup and fails closed when the resource is unavailable).
- [x] Emit external-context retrieval events (`TurnLoop::run_turn_with_external_context`
      emits and durably records the associated model-visible message).

### Stage gate

- [x] RKB evidence can enter context, be compacted to references, and later rehydrate
      (`tests/rkb_integration.rs` gate fixture).
- [x] Source provenance survives all context transformations (core reference round trip,
      runtime/store resume fixture, and RKB citation metadata tests).
- [x] RKB remains an independent project with no core dependency (`pi-rs-rkb` depends on
      generic core/MCP contracts; `pi-rs-core` has no RKB dependency).

---

## Phase 8 — TypeScript extension compatibility host

### Host runtime

- [x] Define host RPC protocol (`pi-rs-extension` uses typed request/response values over
      a private JSON-lines Node boundary; process loss and extension errors are distinct).
- [x] Spawn Node host lazily (construction and an empty module list perform no process I/O;
      `start` is the explicit activation boundary).
- [x] Implement extension process lifecycle (module validation, load, ready/failed/stopped
      status, lifecycle dispatch, explicit shutdown, and process cleanup).
- [x] Isolate extension failures from core process (typed boundary errors; a mutating
      extension tool reports `Unknown` when completion is not observed).

### Extension APIs

- [x] Support tool registration (`pi.registerTool` metadata/schema plus async execution,
      progress events, and a `pi-rs-core::Tool` wrapper).
- [x] Support slash-command registration (`pi.registerCommand` and command dispatch).
- [x] Support selected lifecycle events (`session_start`, `session_shutdown`, `turn_start`,
      `turn_end`, `tool_call`, and `tool_result`).
- [x] Support context hooks (`context` message transformation).
- [x] Support selected TUI hooks (`ctx.ui.notify`, `setStatus`, and `setWidget` as typed
      UI events; full custom widgets remain deferred).
- [x] Add representative compatibility fixtures (`tests/compat/extensions/*.ts` uses the
      documented Pi default factory shape and type-only package import).

### Stage gate

- [x] Representative Pi TypeScript extensions run with minimal/no changes
      (`tests/extension_host.rs`: tool, command, lifecycle, context, and UI dispatch).
- [x] Node is not launched when no compatible extension requires it (empty-host test and
      construction-only API; CI pins/verifies Node 22.x separately).
- [x] Extension failure does not corrupt the core session (hook error remains a typed host
      error, process loss is distinct, host stays usable, mutating tool failure is `Unknown`,
      and a core registry can still be constructed; `tests/extension_host.rs`).

---

## Phase 9 — MCP server / worker mode

### Agent API

- [x] Implement `agent.start` (`pi-rs-mcp::worker::WorkerService` returns a stable session/run
      handle and accepts an optional bounded wait).
- [x] Implement `agent.continue` against the same worker session.
- [x] Implement `agent.cancel` with a per-run cancellation token and terminal cancellation state.
- [x] Implement `agent.branch` for current-state branches; checkpoint/event branch requests are
      explicitly unsupported until an engine supplies historical snapshots (Phase 10).
- [x] Implement `agent.compact` with conversation/phase/checkpoint modes and a context epoch.

### Resources

- [x] Expose session state.
- [x] Expose external summary.
- [x] Expose model-visible messages with bounded sequence pagination.
- [x] Expose a coarse trace projection with ordering, attribution, and provenance only.
- [x] Expose diff availability and typed diff entries when an engine supplies them.
- [x] Expose artifact references and explicit unavailable state.
- [x] Expose latest checkpoint metadata/capsule when an engine supplies one.

### Orchestration semantics

- [x] Support long-running execution semantics (asynchronous run handles, bounded `wait_ms`,
      per-run cancellation, and resource polling).
- [x] Keep internal trace separate from external summaries; no hidden reasoning is inferred.
- [x] Preserve model epochs and failover history in the typed worker state projection supplied
      by the headless engine.
- [x] Keep API coarse and stable (`agent.*` tools and `session://` JSON resources); the stdio
      dispatcher exposes no implementation paths or raw event payloads.

### Stage gate

- [x] External orchestrator can run and inspect a `pi-rs` worker without scraping terminal output
      (`pi-rs-mcp::worker::serve_stdio` and MCP handshake/tools/resources fixtures).
- [x] Worker API does not expose unnecessary internal implementation details (bounded resource
      projections, explicit diff/artifact availability, and typed domain errors).

---

## Phase 10 — Replay and research tooling

### Replay

- [x] Implement read-only `pi-rs replay` over canonical trace/session JSONL.
- [x] Filter by tools.
- [x] Filter by reasoning.
- [x] Filter by timing.
- [x] Replay until event (inclusive, ordered by sequence number).
- [x] Reconstruct model-visible context at an event while retaining canonical history separately.

### Branching

- [x] Plan a dry branch from a historical event without executing it.
- [x] Mark any future execution boundary separately from historical replay.
- [x] Compare two continuations structurally from the same historical state.

### Analysis

- [x] Add model-epoch timeline.
- [x] Add compaction timeline.
- [x] Add failover timeline.
- [x] Add provenance summary without inferring hidden reasoning.
- [x] Add redacted trace export that omits raw payload references/content.

### Stage gate

- [x] Historical execution can be inspected deterministically without confusing replay with new generation.
- [x] Model-visible context can be reconstructed for selected events.

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

## Current priorities — plan of record

> Reconstructed 2026-09-08 after the full merge of the open PR series into `main`
> (492 to 806 tests). The prose list that lived here was never committed and did not
> survive the merge; this section rebuilds it from the merged state, phase by phase.

### P0 — Next

- [x] Pre-emptive reduction producer: a `Compact` policy recommendation at a safe
      boundary now evicts the oldest model-visible turns to the profile's recent
      target, recorded as `ContextReduced` with the target it reached (the durable
      epoch it does not open stays in the trace untouched).
- [x] Summarizing compaction producer: the durable compaction epoch mechanism is
      implemented (`TurnLoop::compact`); request-size/window-pressure heuristic
      producer triggers compaction during the agent loop via `CompactionStrategy::Summarize`
      or pluggable `Summarizer`, and `/compact [notes]` interactive command enables manual
      compaction with epoch tracking.
- [x] Fold retrieved external context into the turn: `ExternalContextItem` supports
      inline and reference payloads, emits `ExternalContextRetrieved` to the event trace,
      and folds formatted context and citations into the model's message path via
      `TurnLoop::run_turn_with_external_context`.
- [x] Slash-completion for the interactive input line, plus its benchmark: `Tab` walks
      `Completions` in `pi-rs-tui::complete`, the editor owns the cycle, and the loop answers
      `/help`, `/quit`, and `/exit` itself.

### P1 — Follow-on

- [x] Load `SKILL.md` bodies and honour `disable-model-invocation`; present skills to
      the model as a skill-control prompt: `Skill::body()` reads one on demand, the block
      Pi offers (`skill::control_prompt`) names only visible skills, and `run`/`interactive`
      send it as the system message; `--show` is the explicit invocation, and
      `--control-prompt` shows exactly what a session would send.
- [x] Invoke a prompt template from inside a session (`/name`): `interactive` discovers
      the scan at open, Tab completes the names, and `prompt::parse_arguments` splits the
      typed string by Pi's editor rule — quotes stripped, empty quotes no argument,
      unclosed quote swallows the rest.
- [x] Checkpoint creation driven by the runtime under context pressure, not only the
      explicit path; document what a checkpoint does to compaction policy:
      `ContextAction::SuggestCheckpoint` triggers automatic capsule synthesis and
      checkpoint barrier reset; documented in Canonical Design §15 and ARCHITECTURE.md §10.
- [x] Import a whole Pi session directory: `import-pi <dir>` files every `*.jsonl` it holds
      directly, each as its own session, and names cross-file lineage as the thing it
      deliberately does not reconstruct.

### P2 — Later / deliberately deferred

- [ ] Windows CI matrix (macOS and Linux are gated).
- [ ] Telemetry, metrics, and analytics surfaces: deferred; privacy and scope decision.
- [ ] GitHub Pages site.
- [x] MCP server/worker mode (Phase 9): the typed `pi-rs-mcp::worker` adapter is implemented;
      provider/runtime composition remains explicit at its `WorkerEngine` boundary. The selected
      TypeScript extension host (Phase 8) is complete above.

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
