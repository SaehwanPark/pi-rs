# pi-rs

<p align="center">
  <strong>A minimal, Pi-inspired coding-agent runtime implemented in Rust.</strong>
</p>

<p align="center">
  <a href="https://github.com/SaehwanPark/pi-rs/actions/workflows/ci.yml"><img src="https://github.com/SaehwanPark/pi-rs/actions/workflows/ci.yml/badge.svg" alt="CI Status" /></a>
  <a href="https://saehwanpark.github.io/pi-rs/"><img src="https://img.shields.io/badge/docs-GitHub%20Pages-blue.svg" alt="Documentation" /></a>
  <a href="https://github.com/SaehwanPark/pi-rs/releases"><img src="https://img.shields.io/badge/release-v0.1.0-green.svg" alt="Release v0.1.0" /></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/rust-1.85%2B%20(2024)-orange.svg" alt="Rust 1.85+" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-purple.svg" alt="License" /></a>
</p>

---

> **Minimal core. Compatible ecosystem. Observable execution. Honest provenance. Recoverable state.**

`pi-rs` preserves the strengths and interaction ergonomics of [Pi](https://github.com/mariozechner/pi) while delivering a zero-dependency Rust runtime with typed event sourcing, honest reasoning provenance, deterministic replay, and sub-millisecond startup times.

---

## Screenshots

### Interactive TUI Mode
Real-time streaming, multi-line editor buffer, syntax-highlighted tool activity, and a persistent semantic statusline:

![pi-rs Interactive TUI](assets/screenshots/interactive-tui.png)

### One-Shot Headless Runner
Streamlined CLI execution with strict stdout/stderr separation and explicit provenance labels (`[native reasoning]`):

![pi-rs CLI Runner](assets/screenshots/cli-run.png)

---

## Core Highlights

- **Interactive TUI**: Keyboard-first terminal pair programming with multi-line editor, live streaming, and semantic statusline.
- **One-Shot CLI Runner (`pi-rs run`)**: Scriptable headless turn execution; pure assistant prose to `stdout`, structured execution telemetry to `stderr`.
- **Honest Provenance**: Distinct attribution for `[native reasoning]`, `[provider summary]`, `[declared]`, and `[reconstructed]` rationale. Hidden chain-of-thought is never falsely claimed.
- **Append-Only Event Store & Replay**: Complete execution timeline stored in `trace.jsonl`; inspect sessions with `pi-rs trace` or replay deterministically with `pi-rs replay` without re-running tools.
- **Workspace Confinement**: File tools reject traversal and outward-pointing symlink components under `--cwd`; mutating tools (`write`, `edit`, `exec`) require explicit configuration approval.
- **Pi Ecosystem Compatibility**: Drop-in discovery for Pi skills, prompt templates, packages, and bidirectional session migration (`import`/`export`).
- **Resilient Model Failover**: Pre-validated backup provider failover with capability checks (tools, modalities, context limits).
- **Lazy MCP Integration**: Stdio Model Context Protocol client initialized on-demand without startup penalties.
- **Sub-Millisecond Startup**: Cold startup <250 ms, warm startup <1 ms.

---

## Quickstart (60 Seconds)

### 1. Install

```bash
# Build and install from source
cargo install --path .

# Or download prebuilt binaries from GitHub Releases
# https://github.com/SaehwanPark/pi-rs/releases
```

### 2. Configure (`config.json`)

Works out of the box with local models (Ollama, vLLM) and OpenAI-compatible cloud providers:

```json
{
  "version": 1,
  "primary": "local/qwen2.5-coder:latest",
  "thinking": "medium",
  "state_dir": ".pi-rs-state",
  "endpoints": [
    {
      "provider": "local",
      "model": "qwen2.5-coder:latest",
      "base_url": "http://localhost:11434/v1",
      "capabilities": {
        "text": true,
        "images": false,
        "tools": true,
        "exposed_reasoning": "native",
        "context_window": 32768
      }
    }
  ],
  "tools": {
    "auto_approve_mutating": true
  }
}
```

### 3. Run

```bash
# One-shot task
pi-rs run --config config.json --cwd . --prompt "Inspect Cargo.toml and list workspace members"

# Interactive terminal session
pi-rs interactive --config config.json
```

---

## CLI Command Cheat Sheet

| Command | Usage | Description |
| :--- | :--- | :--- |
| `run` | `pi-rs run --config <cfg> --cwd <dir> --prompt <txt>` | Run one durable coding-agent turn |
| `interactive` | `pi-rs interactive --config <cfg>` | Start interactive multi-turn terminal UI |
| `trace` | `pi-rs trace <session-id>` | Read chronological event log out of store |
| `replay` | `pi-rs replay <session-id>` | Deterministically replay session without I/O |
| `skills` | `pi-rs skills [--project]` | List skills offered to model |
| `prompts` | `pi-rs prompts [--project]` | List discovered prompt templates |
| `prompt` | `pi-rs prompt <name> [args...]` | Expand prompt template with parameters |
| `packages` | `pi-rs packages` | List discovered Pi packages and surfaces |
| `trust` | `pi-rs trust [list\|allow\|deny]` | Manage project-level trust decisions |
| `compat` | `pi-rs compat <path>` | Audit package/directory for compatibility |
| `import` | `pi-rs import <session.jsonl>` | Import Pi session with loss diagnostics |
| `export` | `pi-rs export <session-id>` | Export session to Pi JSONL format |

---

## Performance Baselines

Enforced in CI via automated benchmark harnesses (`bench/`):

| Operation | Target Budget | Measured CI Baseline |
| :--- | :---: | :---: |
| **Warm Startup** (to interactive) | < 100 ms | **0.35 ms** |
| **Cold Startup** (fresh binary inode) | < 250 ms | **0.80 ms** |
| **Keystroke / Render Latency** | < 16 ms (60 FPS) | **< 2.0 ms** |
| **Slash-Command Completion** | < 50 ms | **< 1.0 ms** |
| **Session Metadata Lookup** | < 50 ms | **< 3.0 ms** |

---

## Documentation

Full public documentation is available on **[GitHub Pages](https://saehwanpark.github.io/pi-rs/)**.

For codebase architecture and developer contracts:
- **[Documentation Index](docs/README.md)** — Master map of all repository documentation.
- **[Canonical Project Design](docs/PROJECT_DESIGN_CANONICAL.md)** — Authoritative source of truth for runtime architecture.
- **[Architecture Boundaries](ARCHITECTURE.md)** — Subsystem invariants and crate boundaries.
- **[Pi Compatibility Guide](COMPATIBILITY.md)** — Compatibility coverage, tests, and fixture inventory.
- **[Contributing Guide](CONTRIBUTING.md)** — Developer workflows, quality checks, and PR slicing.
- **[Roadmap](ROADMAP.md)** — Milestone tracker and active phases.

---

## License

`pi-rs` is distributed under the [MIT License](LICENSE).
