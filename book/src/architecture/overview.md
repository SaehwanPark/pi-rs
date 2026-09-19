# System Architecture

The `pi-rs` workspace is organized into discrete, layered Rust crates with explicit architectural boundaries:

```text
+-----------------------------------------------------------------+
|                             pi-rs                               |
|                     (CLI binary & surfaces)                     |
+-----------------------------------------------------------------+
          |                                      |
          v                                      v
+-------------------+                  +-------------------+
|   pi-rs-runtime   | <--------------> |     pi-rs-tui     |
|   (Turn loop)     |                  | (Terminal render) |
+-------------------+                  +-------------------+
  |        |       \
  |        |        \-------------------------------\
  v        v                                         v
+---------------+  +------------------+     +------------------+
| pi-rs-provider|  |   pi-rs-tools    |     |   pi-rs-store    |
| (LLM adapters)|  | (read/write/exec)|     | (Append trace)   |
+---------------+  +------------------+     +------------------+
          \                 /                        /
           v               v                        v
     +---------------------------------------------------+
     |                    pi-rs-core                     |
     |         (Typed events, provenance, errors)        |
     +---------------------------------------------------+
```

---

## Crate Responsibilities

| Crate | Boundary Role |
| :--- | :--- |
| `pi-rs-core` | Foundational vocabulary: `AgentEvent`, `SessionRecord`, `ProvenanceKind`, `ModelEpoch`, common error contracts. Zero I/O, zero network, zero unsafe. Compiles independently. |
| `pi-rs-store` | Durable append-only event logging (`trace.jsonl`), session index, blob storage. Thread-safe, lock-free reader patterns. |
| `pi-rs-provider` | `ModelProvider` normalization and the OpenAI-compatible HTTP/SSE adapter used for local and remote endpoints. |
| `pi-rs-tools` | Workspace-confined tool execution engine (`read`, `write`, `edit`, `grep`, `exec`) with bounded output and explicit mutation state. |
| `pi-rs-runtime` | Turn state machine, retry loop, primary/backup failover coordinator, and context engine. |
| `pi-rs-tui` | Terminal presentation: ANSI-free semantic line generation, column measurement, live streaming, statusline. |
| `pi-rs-compat` | Upstream Pi ecosystem compatibility: skills, templates, packages, and trust store. |
| `pi-rs-mcp` | Lazy stdio and bounded Streamable HTTP MCP transports, discovery, tool normalization, and the typed worker boundary. |
| `pi-rs-replay` | Deterministic session playback and forensic examination. |
| `pi-rs-rkb` | Provenance-aware external context and retrieval reference integration. |
| `pi-rs-extension` | Optional Node/TypeScript Pi extension host behind a typed JSON-lines boundary. |
| `pi-rs-experiments` | Opt-in, pure adaptive-context, standby, and MCP exposure measurements. |

---

## The Core Invariant: State vs Presentation Separation

A strict boundary exists between the terminal-native presentation layer (`pi-rs-tui`) and the runtime/store:
- `pi-rs-tui` answers only one question: *how does an `AgentEvent` render on a terminal of $N$ columns?*
- The renderer has no access to sockets, filesystem paths, or runtime mutators. A presentation bug can never mutate session state.
