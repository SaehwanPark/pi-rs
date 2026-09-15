# Handoff — Phase 11 optimization experiments

Date: 2026-09-15
Branch: `agent/phase-11-optimization`
Base: `main` at `0d46bd5` (Phase 10 merged as PR #82)

## Stop reason

The configured Codex usage monitor reported 100% usage, above the repository policy stop
threshold (>=97%). No Phase 11 implementation files were changed. Stop before further
substantive work or verification; resume after the usage window is available.

## Completed before stop

- Phase 10 replay was reviewed, fixed, CI-verified, and merged to `main`.
- Phase 11 branch was created and pushed.
- Phase 11 discovery completed. The recommended smallest verified slice is:
  - optional request first-event/TTFT timing (`first_delta_ms`) with backward-compatible
    trace fields;
  - pure, model/runtime-specific context-latency samples and knee detection;
  - adaptive thresholds that only lower/cap profile-derived thresholds and are disabled by
    default;
  - a deterministic context/prefill benchmark with static-profile comparison;
  - optional warm backup-adapter measurement (startup/RSS trade-off), retaining cold lazy
    backup as the default;
  - predictive/lazy MCP capability-prefetch measurement, retaining minimal schema exposure
    as the default.

## Existing seams

- Context profiles and thresholds: `crates/pi-rs-core/src/context.rs`
  (`ContextProfile`, `ContextThresholds`, `ProfilePolicy`).
- Request context estimate: `ModelRequest::estimate_tokens` in
  `crates/pi-rs-core/src/provider.rs`.
- Request lifecycle events: `ModelRequestStarted`/`ModelRequestCompleted` in
  `crates/pi-rs-core/src/event.rs`; runtime request handling in
  `crates/pi-rs-runtime/src/turn.rs`.
- Runtime config and policy construction: `crates/pi-rs-core/src/config.rs` and
  `src/run.rs`.
- Cold backup boundary: `pi_rs_provider::Deferred` and `backup_provider` in `src/run.rs`;
  do not eagerly initialize it.
- Lazy MCP activation and first-use latency: `crates/pi-rs-mcp/src/manager.rs`.
- Existing benchmark wrappers: `bench/startup.sh`, `bench/large_session.sh`,
  `bench/render.sh`; JSON artifacts are ignored by `bench/results/.gitignore` unless force-added.

## Suggested next loop

1. Check Codex usage before spawning agents or coding. Keep the repository's >=97% stop policy.
2. Implement optional backward-compatible first-event timing and focused runtime/core tests.
3. Add a pure `pi-rs-experiments` (or equivalent bounded) crate/API for observations, knee
   detection, adaptive recommendation, static comparison, backup measurements, and MCP exposure
   plans. Label fixture/proxy evidence honestly; do not claim real prefill without provider data.
4. Add `bench/context_prefill.sh`, backup-standby and MCP exposure measurements as appropriate,
   with deterministic fixture tests and real-provider mode clearly optional.
5. Keep defaults unchanged: balanced context profile, adaptive mode off, cold backup, minimal
   MCP exposure, no startup/network/process work on normal startup.
6. Update `ROADMAP.md`, `ARCHITECTURE.md`, `docs/PROJECT_DESIGN_CANONICAL.md`,
   `docs/IMPLEMENTATION_STATUS.md`, and a `_workspace/changes/phase-11-optimization/00_change-brief.md`
   only after focused/full verification and invariant review.
7. Open a Phase 11 PR, require green Ubuntu/macOS CI, merge to `main`, then mark the Phase 11
   gate complete only from recorded benchmark and test evidence.

## Repository state

The branch is clean except for intentionally untracked goal coordination files:
`.pi/.goals-pool-snapshot.json` and `.pi/goals/`. Do not add those files.
