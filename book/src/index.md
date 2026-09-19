# Introduction to pi-rs

`pi-rs` is a minimal, Pi-inspired coding-agent runtime implemented in Rust. The
current public release is **v0.2.0 (2026-09-19)**.

It delivers a fast, trustworthy runtime for coding-agent sessions that preserves the interaction ergonomics of Pi while establishing rigorous architectural boundaries for provenance, determinism, and performance.

---

## Project Thesis

> **Minimal core. Compatible ecosystem. Observable execution. Honest provenance. Recoverable state.**

The project is a **clean reimplementation**, not a mechanical translation or fork. Rust is the implementation substrate, delivering predictable abstractions, thread safety, and measured low-latency startup.

---

## Core Principles

- **Preserve Pi's minimalist philosophy**: Keep the user-facing layer focused and keyboard-native; push higher-level orchestration outside the runtime.
- **Context is a cache, not the record**: The append-only canonical trace remains completely independent of transient model-visible context.
- **Honest provenance**: Never conflate raw model reasoning, provider-synthesized summaries, declared rationale, or reconstructed explanations. Never claim hidden chain-of-thought was recovered unless truly exposed.
- **Single-model execution**: Exactly one model is active in normal operation; backup models are reserved strictly for fault recovery.
- **Explicit mutating state**: Uncertain mutating operations (`write`, `edit`, `exec`) remain explicitly flagged and are never silently or blindly replayed.
- **Instant before complete**: Subsystems such as MCP, Node extension hosts, and deep session hydration initialize lazily to target startup budgets under 100 ms warm and under 250 ms cold.

---

## Key Highlights

| Feature | Description |
| :--- | :--- |
| **Interactive TUI** | Keyboard-first terminal UI with multi-line editor, real-time streaming, and semantic statusline. |
| **One-Shot CLI** | Headless runner (`pi-rs run`) streaming output directly to stdout/stderr with strict workspace confinement. |
| **Deterministic Replay** | Replay any historical session identically without re-querying providers or re-executing mutating tools. |
| **Pi Compatibility** | Tested behavioral support for Pi skills, prompt templates, packages, selected extensions, and session import/export. |
| **Model Failover** | Automatic fallback to backup models with capability validation (tools, modalities, context limits). |
| **MCP Integration** | Model Context Protocol client with lazy stdio initialization. |
| **Low-Latency Startup** | Cold startup <250 ms, warm startup <100 ms budget. |

---

## Quick Navigation

- [Installation](getting-started/installation.md): Build from source or install via Cargo.
- [Quickstart Guide](getting-started/quickstart.md): Run your first session in 60 seconds.
- [Interactive TUI Guide](user-guide/interactive-tui.md): Learn the keyboard commands and editor buffer.
- [CLI Reference](reference/cli.md): Comprehensive listing of all flags and options.
