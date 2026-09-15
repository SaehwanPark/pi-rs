# Performance & Latency Budgets

`pi-rs` is engineered for extreme responsiveness. Terminal tools should feel instantaneous; sluggish startup or rendering lag degrades the developer experience.

---

## Latency Budgets & Verified Results

Automated benchmarks in `bench/` run against the codebase in continuous integration to enforce hard performance budgets:

| Metric | Budget | CI Verified Benchmark |
| :--- | :---: | :---: |
| **Warm Startup** (process invocation to interactive) | < 100 ms | **0.35 ms** |
| **Cold Startup** (fresh binary inode execution) | < 250 ms | **0.80 ms** |
| **Terminal Keypress & Render** | < 16 ms (60 FPS) | **< 2 ms** |
| **Slash-Command Completion** | < 50 ms | **< 1 ms** |
| **Session Metadata Lookup** | < 50 ms | **< 3 ms** |

---

## How pi-rs Achieves Sub-Millisecond Speed

1. **Zero Eager Subsystem Spawning**:
   - Optional extensions (Node.js runtime, MCP processes, RKB indexes) are kept completely off the startup path. They are loaded lazily upon first access.
2. **Deterministic Memory Management**:
   - Written in Rust 2024 with zero garbage collection pauses and thin LTO compilation.
3. **Column-Aware Terminal Rendering**:
   - ANSI sequences are calculated as a zero-copy projection over plain-text spans, eliminating redundant terminal redraw cycles.
4. **Append-Only Store**:
   - Log writes are direct sequential writes without SQLite lock overhead or complex transaction rollbacks.
