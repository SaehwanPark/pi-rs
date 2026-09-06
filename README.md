# pi-rs

`pi-rs` is a minimal, Pi-inspired coding-agent runtime implemented in Rust.

It aims to preserve the strengths of Pi's interaction model and ecosystem while adding first-class runtime support for:

- observable execution and explicit reasoning provenance;
- long-running context lifecycle management;
- local and remote models;
- primary/backup model failover;
- MCP client/server interoperability;
- provenance-aware external context;
- replayable and inspectable execution;
- low-latency startup and responsive terminal UX.

## Project thesis

> Minimal core. Compatible ecosystem. Observable execution. Honest provenance. Recoverable state.

The project is a **clean reimplementation**, not a source rewrite or fork.

Rust is the implementation substrate, not the main differentiator.

## Core principles

- Preserve Pi's minimalist agent philosophy where practical.
- Treat context as a cache, not the canonical record.
- Keep the execution trace separate from model-visible context.
- Record reasoning provenance explicitly.
- Never claim to recover hidden chain-of-thought that was not exposed.
- Keep only one active model in a normal execution role.
- Treat backup-model activation as fault recovery, not orchestration.
- Keep higher-level orchestration outside the runtime.
- Make optional capabilities lazy by default.
- Prefer semantic visual hierarchy over decorative UI.
- Optimize context for useful information per unit inference cost, not maximum fill.

## Intended architecture

```text
+---------------------------------------------------------+
|                         pi-rs                           |
|                                                         |
|   Agent Loop <----> Provider Abstraction                |
|        |                                                |
|        v                                                |
|     Event Bus                                           |
|        |                                                |
|   +----+----------+----------------+                    |
|   |               |                |                    |
| Session Store   Context Engine   Trace/Replay           |
|                     |                                    |
|                Artifacts / External Context             |
|                     |                                    |
|                 MCP / Extensions                        |
+---------------------+-----------------------------------+
                      |
             external systems
```

## One-shot agent command

Run one complete, durable turn against an explicitly configured OpenAI-compatible
endpoint:

```text
pi-rs run --config <file> --cwd <workspace> --prompt <text>
```

The JSON file is parsed as `RuntimeConfig`; no project-local config is discovered.
`--cwd` is canonicalized and becomes the confinement root for the built-in tools.
Assistant text is streamed to stdout. Provenance-labeled reasoning, tool activity,
diagnostics, and errors use stderr. Mutating tools, including `write`, `edit`, and
`exec`, are refused unless the config explicitly sets
`tools.auto_approve_mutating` to `true`. Outside-workspace file reads are denied
for this command. An explicitly approved `exec` still invokes a shell and is an
intentional escape hatch, not an OS sandbox; use operating-system isolation when
untrusted commands require containment.

Each invocation creates a session under `state_dir` and persists attributed user,
assistant, and tool messages separately from the ordered canonical trace. The
command does not provide an interactive approval prompt, session resume, or a REPL.

## Major runtime capabilities

### Provider abstraction

Support local and remote models behind a common runtime contract.

Early targets should include:

- one local/OpenAI-compatible provider path;
- one remote/cloud-compatible provider path;
- pluggable custom providers.

### Event trace and provenance

Important runtime activity should become typed events:

- model requests;
- native reasoning;
- assistant output;
- tool lifecycle;
- context compaction;
- external-context retrieval;
- retries;
- failover;
- checkpoints.

### Context lifecycle

The runtime should distinguish:

1. forensic trace;
2. working model context;
3. durable semantic state.

Compaction may shrink the working set while preserving the canonical trace.

### Model failover

Users may configure one optional backup model.

The runtime retries eligible transient failures first, then fails over only when the active model cannot reliably continue.

A configured backup adapter is built the first time a request actually needs it. Configuring a backup that is never used costs nothing, and a backup that cannot be built fails at the moment it is needed, naming itself.

### MCP

`pi-rs` should:

- consume MCP tools/resources as a client;
- expose itself as an MCP-accessible worker later;
- avoid eagerly injecting every MCP tool into model context.

### Pi compatibility

Compatibility should be explicit and tested.

Priority targets:

1. skills;
2. prompt templates;
3. package discovery/install;
4. session import/export;
5. extension tools/commands;
6. selected lifecycle and UI compatibility.

### External context

Durable knowledge should be representable through rehydratable references rather than copied permanently into working context.

`rkb-rs` is the first planned reference integration.

## UX philosophy

> Quiet by default. Rich when inspected. Semantic rather than decorative.

The TUI should:

- feel familiar to Pi users;
- remain terminal-native and keyboard-first;
- syntax-highlight operations, arguments, paths, and prompts;
- visually distinguish reasoning provenance;
- surface rare events such as failover and emergency compaction clearly;
- collapse verbose detail by default;
- avoid dashboard-style permanent chrome.

## Performance philosophy

> Instant before complete.

The runtime should become interactive before optional subsystems finish initialization.

Use lazy loading for:

- Node extension host;
- MCP connections;
- backup model initialization;
- deep session hydration;
- heavy external indexes;
- optional package implementations.

Aspirational early targets:

```text
warm startup to interactive       <100 ms
cold startup to interactive       <250 ms
keypress/render latency             <16 ms
slash completion                     <50 ms
local session metadata lookup       <50 ms
```

These are engineering targets, not compatibility promises.

## Repository documents

- `ARCHITECTURE.md` — implementation boundaries and runtime invariants.
- `COMPATIBILITY.md` — Pi compatibility targets and support policy.
- `ROADMAP.md` — staged implementation plan and tracked action items.
- `AGENTS.md` — instructions and constraints for coding agents working in this repository.
- `docs/PROJECT_DESIGN_CANONICAL.md` — source-of-truth project design.

## Initial implementation rule

Do not block a usable MVP on complete ecosystem compatibility or advanced research tooling.

The first objective is:

> Build a small, fast, useful coding agent with trustworthy event semantics.

Everything else should grow from that foundation.
