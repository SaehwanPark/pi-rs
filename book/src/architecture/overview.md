# System Architecture

The `rupi` workspace is organized into discrete, layered Rust crates with explicit architectural boundaries:

```text
+-----------------------------------------------------------------+
|                             rupi                               |
|                     (CLI binary & surfaces)                     |
+-----------------------------------------------------------------+
          |                                      |
          v                                      v
+-------------------+                  +-------------------+
|   rupi-runtime   | <--------------> |     rupi-tui     |
|   (Turn loop)     |                  | (Terminal render) |
+-------------------+                  +-------------------+
  |        |       \
  |        |        \-------------------------------\
  v        v                                         v
+---------------+  +------------------+     +------------------+
| rupi-provider|  |   rupi-tools    |     |   rupi-store    |
| (LLM adapters)|  | (read/write/exec)|     | (Append trace)   |
+---------------+  +------------------+     +------------------+
          \                 /                        /
           v               v                        v
     +---------------------------------------------------+
     |                    rupi-core                     |
     |         (Typed events, provenance, errors)        |
     +---------------------------------------------------+
```

---

## Crate Responsibilities

| Crate | Boundary Role |
| :--- | :--- |
| `rupi-core` | Foundational vocabulary: `AgentEvent`, `SessionRecord`, `ProvenanceKind`, `ModelEpoch`, common error contracts. Zero I/O, zero network, zero unsafe. Compiles independently. |
| `rupi-store` | Durable append-only event logging (`trace.jsonl`), session index, blob storage. Thread-safe, lock-free reader patterns. |
| `rupi-provider` | `ModelProvider` normalization and the OpenAI-compatible HTTP/SSE adapter used for local and remote endpoints. |
| `rupi-tools` | Workspace-confined tool execution engine (`read`, `write`, `edit`, `grep`, `exec`) with bounded output and explicit mutation state. |
| `rupi-runtime` | Turn state machine, retry loop, primary/backup failover coordinator, and context engine. |
| `rupi-tui` | Terminal presentation: ANSI-free semantic line generation, column measurement, live streaming, statusline. |
| `rupi-compat` | Upstream Pi ecosystem compatibility: skills, templates, packages, and trust store. |
| `rupi-mcp` | Lazy stdio and bounded Streamable HTTP MCP transports, discovery, tool normalization, and the typed worker boundary. |
| `rupi-replay` | Deterministic session playback and forensic examination. |
| `rupi-rkb` | Provenance-aware external context and retrieval reference integration. |
| `rupi-extension` | Optional Node/TypeScript Pi extension host behind a typed JSON-lines boundary. |
| `rupi-experiments` | Opt-in, pure adaptive-context, standby, and MCP exposure measurements. |

---

## The Core Invariant: State vs Presentation Separation

A strict boundary exists between the terminal-native presentation layer (`rupi-tui`) and the runtime/store:
- `rupi-tui` answers only one question: *how does an `AgentEvent` render on a terminal of $N$ columns?*
- The renderer has no access to sockets, filesystem paths, or runtime mutators. A presentation bug can never mutate session state.
