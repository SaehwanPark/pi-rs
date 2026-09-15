# Changelog

All notable changes to this project will be documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [0.1.0] - 2026-09-15

### Added

#### Core Runtime & Presentation
- **Interactive TUI (`pi-rs interactive`)**: High-performance terminal user interface powered by `ratatui` and `crossterm`, featuring a multi-line editing buffer, ANSI-clean line rendering, live stream display, and real-time statusline.
- **One-Shot Runner (`pi-rs run`)**: Headless command-line runner with strict stream separation (assistant answer on `stdout`; provenance, tool lifecycle, and diagnostics on `stderr`).
- **Workspace Confinement**: Strict realpath confinement to `--cwd` for built-in file operations (`read`, `write`, `edit`). Mutating operations require explicit `auto_approve_mutating` configuration.
- **Durable Event Store**: Append-only event logging (`trace.jsonl`), session index, and blob storage under `.pi-rs-state`.
- **Deterministic Replay (`pi-rs replay`)**: Replay recorded sessions identically to live runs without network calls or mutating side-effects.
- **Trace Inspection (`pi-rs trace`)**: Chronological structured event log viewer.

#### Honest Reasoning Provenance
- Explicit typed labels for reasoning streams:
  - `[native reasoning]`: authentic model thinking tokens.
  - `[provider summary]`: provider-generated summaries of hidden reasoning.
  - `[declared]`: explicitly declared rationale.
  - `[reconstructed]`: post-hoc recovered explanation.
- Invariant enforced: hidden chain-of-thought is never falsely claimed as recovered.

#### Model Providers & Failover
- Unified provider abstraction supporting local endpoints (Ollama, vLLM, LM Studio) and cloud endpoints (OpenAI, DeepSeek, Together).
- Resilient primary/backup failover with pre-flight capability matching (tools, modalities, context limits) and lazy adapter initialization.

#### Pi Ecosystem Compatibility
- **Skills (`pi-rs skills`)**: Drop-in discovery for user (`~/.pi/agent/skills`, `~/.agents/skills`) and project-level skills (`.pi/skills`, `.agents/skills`) guarded by explicit `--project` trust checks.
- **Prompt Templates (`pi-rs prompts`, `pi-rs prompt`)**: Template discovery and positional argument expansion (`$1`, `$@`, `${1:-default}`).
- **Packages (`pi-rs packages`)**: Manifest discovery and local package installation.
- **Session Migration (`pi-rs import`, `pi-rs export`)**: Loss-diagnostic bidirectional migration between `pi-rs` store and Pi JSONL formats.
- **Trust Store (`pi-rs trust`)**: Per-project allow/deny trust record management.
- **Compatibility Diagnostics (`pi-rs compat`)**: Compatibility auditing for external packages and artifacts.

#### Integrations & Latency
- **Model Context Protocol (MCP)**: Lazy stdio client architecture initializing servers on demand.
- **Verified Latency Baselines**: Warm startup < 1 ms (measured 0.35 ms), cold startup < 250 ms (measured 0.80 ms), keystroke latency < 16 ms (measured < 2 ms).

#### Documentation & Public Site
- Comprehensive public documentation deployed via mdBook on GitHub Pages.
- Clean reorganization of internal/historical slices into `docs/archive/`.
- Modern, concise README with representative screenshots.
