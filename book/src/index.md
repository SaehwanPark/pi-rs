# Welcome to rupi

**rupi** is a small terminal coding harness. It connects a model to a workspace, lets the
model inspect files and (when you allow it) change them, and records what happened so you
can inspect or resume the session later.

You do **not** need to know Claude Code, Codex, Pi, Rust, or agent terminology to try it.
Think of rupi as three things:

1. a terminal program you start;
2. a model endpoint that supplies the responses;
3. a guarded set of tools for reading, editing, and testing files.

rupi is an independent project, not a rewrite or Rust port of Pi. It does reuse selected
Pi-compatible skills, prompts, packages, extensions, and session formats when that is useful.

## The shortest path to a first answer

1. [Install rupi](getting-started/installation.md).
2. [Start a local Qwen3.8-Flash-Next server](getting-started/quickstart.md#1-start-llamacpp).
3. [Create a safe configuration](getting-started/quickstart.md#2-create-configjson).
4. [Ask a read-only question](getting-started/quickstart.md#3-run-a-read-only-request).
5. [Resume, inspect, or replay the session](getting-started/quickstart.md#5-find-and-reuse-a-session).
6. [Try the Test Ledger live example](getting-started/test-ledger.md).

The default configuration does **not** approve file writes, edits, or shell commands.
Start with a read-only prompt, then decide deliberately whether a trusted workspace should
allow mutations.

## Useful mental model

```text
You ──> rupi ──> local or remote model
          │
          ├── read-only tools by default
          ├── explicit mutation policy
          └── durable session + trace in .rupi-state/
```

The model's current prompt is temporary working context. The trace is the durable record.
Compaction can shorten the next prompt without erasing the recorded history.

## Where to go next

- [Installation](getting-started/installation.md)
- [Quickstart](getting-started/quickstart.md)
- [Test Ledger live example](getting-started/test-ledger.md)
- [Interactive terminal](user-guide/interactive-tui.md)
- [CLI reference](reference/cli.md)
- [Models and providers](user-guide/models-and-providers.md)
- [Tools and safety](user-guide/tools-and-sandbox.md)
- [Trace and replay](user-guide/trace-and-replay.md)
- [Pi compatibility](user-guide/pi-compatibility.md)

For runtime contracts and design decisions, see [Architecture](architecture/overview.md) and
the repository's [canonical design](../../docs/PROJECT_DESIGN_CANONICAL.md).
