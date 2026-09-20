# Changelog

All notable changes to this project will be documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

### Added

- Added an opt-in model progress boundary for bounded implementation turns. It can
  narrow the next request to configured mutating tools after repeated inspection-only
  tool requests while preserving normal tool lifecycle and `Unknown` semantics.

---

## [0.2.2] - 2026-09-20

This release makes the first installation and first-project path usable without requiring
Rust when a supported prebuilt archive is available.

### Added

- Added checksum-verified `install.sh` for Linux/macOS and `install.ps1` for Windows.
- Added release archives for Linux x86_64, macOS Intel/Apple Silicon, and Windows x86_64,
  plus a tag-driven GitHub Actions publication workflow.
- Added the Test Ledger live example and its independent subprocess acceptance walkthrough.

### Documentation

- Updated the README and mdBook installation guide with installer, source-build, and
  checksum/recovery instructions.
- Added the Test Ledger first-task walkthrough covering safe inspection, bounded changes,
  independent tests, trace, and replay.

---

## [0.2.1] - 2026-09-20

`rupi` is the new project identity. This release clarifies that the runtime is an
independent implementation with selected Pi compatibility, not a Pi rewrite or Rust port.

### Changed

- Renamed the GitHub repository, binary, workspace crates, state/config examples,
  environment variables, CI commands, and public URLs from the former project identity to
  `rupi`.
- Updated the beginner guide, examples, and screenshots for the real llama.cpp endpoint
  at `127.0.0.1:8000` serving `qwen3.8-flash-next`; the local smoke evidence covers provider,
  one-shot run, trace, and replay.
- Curated resolved audit and proposal material into explicit historical archives.
- Made `import-pi` the documented session-import command while retaining `import` as a
  compatibility alias.
- Removed one redundant CLI stdout assertion after reviewing the suite; boundary, failure,
  provenance, compatibility, and recovery coverage remains intact.

### Fixed

- Preserved the request-line separator in the cancellable HTTP relay so strict local
  llama.cpp servers receive a valid `HTTP/1.1` request.

### Migration

Update scripts and local configuration to call `rupi` and use `.rupi-state`. Upstream Pi
paths such as `.pi/skills` and Pi JSONL session files remain unchanged for compatibility.

## [0.2.0] - 2026-09-19

`v0.2.0` packages the audited mainline after the Round 9 sign-off. The audit cycle
closed with no remaining P0 or P1 findings; the release focuses on crash recovery,
bounded transports, conservative tool state, and clearer public documentation.

### Added

- Durable audit evidence for the completed Round 9 review.
- Conservative recovery across session leases, legacy-store migration, checkpoint
  projections, and model-visible message reconstruction.
- Explicit replay and resume boundaries for externalized payloads and uncertain tool
  calls, preserving `Unknown` rather than guessing that a mutating operation failed or
  succeeded.
- Bounded and cancellable provider/MCP relay behavior, including bounded DNS worker
  capacity and fail-closed handling for interrupted requests.

### Fixed

- Closed interrupted tool-failure transactions so the projection WAL cannot remain open
  after a decoded but unexecuted call.
- Preserved exact tool-request identities and parentage through failed provider streams
  and immediate session restore.
- Hardened filesystem, redaction, output-bound, cancellation, and Windows portability
  paths covered by the audit rounds.

### Documentation

- Updated the workspace and lockfile to version `0.2.0`.
- Refreshed the README, mdBook guides, CLI examples, compatibility notes, and release
  documentation for the current command and configuration surfaces.

## [0.1.0] - 2026-09-15

### Added

#### Core Runtime & Presentation
- **Interactive TUI (`rupi interactive`)**: High-performance terminal user interface with a terminal-native semantic renderer and `crossterm`, featuring a multi-line editing buffer, ANSI-clean line rendering, live stream display, and real-time statusline.
- **One-Shot Runner (`rupi run`)**: Headless command-line runner with strict stream separation (assistant answer on `stdout`; provenance, tool lifecycle, and diagnostics on `stderr`).
- **Workspace Confinement**: Strict realpath confinement to `--cwd` for built-in file operations (`read`, `write`, `edit`). Mutating operations require explicit `auto_approve_mutating` configuration.
- **Durable Event Store**: Append-only event logging (`trace.jsonl`), session index, and blob storage under `.rupi-state`.
- **Deterministic Replay (`rupi replay`)**: Replay recorded sessions identically to live runs without network calls or mutating side-effects.
- **Trace Inspection (`rupi trace`)**: Chronological structured event log viewer.

#### Honest Reasoning Provenance
- Explicit typed labels for reasoning streams:
  - `[native reasoning]`: authentic model thinking tokens.
  - `[provider summary]`: provider-generated summaries of hidden reasoning.
  - `[declared]`: explicitly declared rationale.
  - `[reconstructed]`: post-hoc recovered explanation.
- Invariant enforced: hidden chain-of-thought is never falsely claimed as recovered.

#### Model Providers & Failover
- Unified provider abstraction supporting OpenAI-compatible local endpoints (including llama.cpp)
  and cloud endpoints.
- Resilient primary/backup failover with pre-flight capability matching (tools, modalities, context limits) and lazy adapter initialization.

#### Pi Ecosystem Compatibility
- **Skills (`rupi skills`)**: Behavioral-compatible discovery for user (`~/.pi/agent/skills`, `~/.agents/skills`) and project-level skills (`.pi/skills`, `.agents/skills`) guarded by explicit `--project` trust checks.
- **Prompt Templates (`rupi prompts`, `rupi prompt`)**: Template discovery and positional argument expansion (`$1`, `$@`, `${1:-default}`).
- **Packages (`rupi packages`)**: Manifest discovery and local package installation.
- **Session Migration (`rupi import`, `rupi export`)**: Loss-diagnostic bidirectional migration between `rupi` store and Pi JSONL formats.
- **Trust Store (`rupi trust`)**: Per-project allow/deny trust record management.
- **Compatibility Diagnostics (`rupi compat`)**: Compatibility auditing for external packages and artifacts.

#### Integrations & Latency
- **Model Context Protocol (MCP)**: Lazy stdio client architecture initializing servers on demand.
- **Verified v0.1.0 latency evidence**: Warm startup was measured at 0.35 ms, cold startup at 0.80 ms, and keystroke latency below 2 ms on the release host; current budgets and evidence commands are documented in the v0.2.0 guide.

#### Documentation & Public Site
- Comprehensive public documentation deployed via mdBook on GitHub Pages.
- Clean reorganization of internal/historical slices into `docs/archive/`.
- Modern, concise README with representative screenshots.
