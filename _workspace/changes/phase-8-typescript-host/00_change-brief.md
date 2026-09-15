# Phase 8 TypeScript extension host

## Goal

Implement the smallest Pi-compatible TypeScript extension host vertical slice: a lazy,
line-oriented Node RPC boundary that can load trusted `.ts`/`.js` extension modules, expose
registered tools and slash commands, dispatch selected lifecycle/context hooks, and report
selected TUI notifications/status/widgets. Keep extension failures outside the core session
and preserve `ToolExecutionState::Unknown` when a mutating extension call loses its process
completion.

## Owned paths

- `crates/pi-rs-extension/` (new optional host/protocol/tool adapter crate)
- `crates/pi-rs-compat/` extension surface diagnostics and package status
- `tests/compat/extensions/` and focused integration tests
- root workspace wiring and Phase 8 documentation/ROADMAP evidence

## Acceptance evidence

- constructing/configuring the host performs no Node/process I/O;
- no configured extension means no Node launch;
- representative Pi-style TypeScript fixture registers a tool and command, dispatches a
  lifecycle event, transforms context, and emits selected UI events;
- host/protocol/extension failures are typed and isolated; a core session remains usable;
- compatibility diagnostics distinguish supported selected APIs from partial/unsupported
  surfaces and preserve internal-import warnings.

## Non-goals

- arbitrary npm/dependency installation or network package resolution;
- full Pi UI widget parity, provider registration, shortcuts/flags, or undocumented APIs;
- changing core event/session semantics solely for extension compatibility;
- automatic execution of untrusted project-local extensions.

## Verification plan

Focused host protocol/fixture tests, compatibility fixture tests, workspace fmt/check/clippy/
test/doc, startup benchmark (host remains lazy), and invariant review covering subprocess
trust, lifecycle uncertainty, session/provenance isolation, and startup latency.
