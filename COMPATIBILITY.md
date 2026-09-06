# COMPATIBILITY

## 1. Purpose

`pi-rs` is heavily inspired by Pi and should support a meaningful subset of the Pi ecosystem.

Compatibility must be:

- explicit;
- versioned;
- testable;
- documented by surface;
- honest about unsupported behavior.

Do not use a blanket claim such as "fully Pi-compatible."

## 2. Compatibility philosophy

The project targets **behavioral compatibility**, not source-level implementation equivalence.

A Pi artifact is considered compatible when users can reasonably reuse it without rewriting its conceptual behavior.

Compatibility priorities are driven by ecosystem value and implementation cost.

## 3. Compatibility levels

Use the following status vocabulary:

- `Supported` — expected to work and covered by tests.
- `Partial` — useful subset works; limitations are documented.
- `Experimental` — available but unstable or incomplete.
- `Unsupported` — intentionally not supported.
- `Unknown` — not yet evaluated.

## 4. Initial target matrix

| Surface | Initial target |
|---|---|
| `SKILL.md` skills | Supported |
| Prompt templates | Supported |
| Package discovery | Supported |
| Package installation | Supported |
| Package manifests | Supported |
| Session import | Partial |
| Session export | Partial |
| Themes | Partial |
| Extension `registerTool` | Partial -> Supported |
| Extension `registerCommand` | Partial -> Supported |
| Extension lifecycle events | Partial |
| Extension context hooks | Experimental |
| Complex custom TUI | Experimental |
| Arbitrary Pi internal imports | Unsupported |
| Undocumented Pi internals | Unsupported |

## 5. Skills

Skills are a high-priority compatibility target.

Requirements:

- preserve standard `SKILL.md` structure;
- support package-local and project-local discovery where practical;
- retain relative resource references;
- preserve user-visible semantics;
- test representative real-world skills.

Skills should not require the Node compatibility host unless they explicitly depend on executable TypeScript/JavaScript behavior.

### 5.1 Implemented

`pi-rs skills [--project]` scans, in order: `$HOME/.pi/agent/skills`,
`$HOME/.agents/skills`, then `<ancestor>/.pi/skills` and `<ancestor>/.agents/skills` from
the working directory up to the git root. Within a location, a directory containing
`SKILL.md` is a skill; a root `*.md` file is a skill in the `.pi` family and is ignored
in the shared `.agents` family, where only files inside a grouping directory count. The
frontmatter subset read is `name` (required), `description` (required — a skill that
cannot say what it does is never offered), `license`, `compatibility`, `allowed-tools`,
`disable-model-invocation`, including quoted scalars and `|`/`>` block scalars. Project
locations are read only with `--project`; walks are depth-bounded and do not follow
symlinks.

Not yet: package-local skills, the `skills` array in settings, `--skill` paths,
skill-body activation, and the model-facing prompt listing.

## 6. Prompt templates

Prompt templates should preserve:

- naming;
- discovery;
- invocation;
- expected variable behavior where documented.

Prompt compatibility should remain independent from the runtime provider implementation.

## 7. Packages

Support Pi-style package discovery and installation as early as practical.

Goals:

- recognize compatible package manifests;
- install package dependencies when required;
- expose contained skills/prompts/extensions;
- report unsupported package surfaces clearly.

A package should not be considered incompatible merely because one optional feature is unsupported.

Prefer per-surface diagnostics.

Example:

```text
Package compatibility

✓ skills
✓ prompts
✓ registerTool
△ custom UI
✗ internal Pi module import
```

## 8. TypeScript extensions

Existing Pi TypeScript extensions should run through a compatibility host when practical.

Architecture:

```text
pi-rs
  |
extension RPC
  |
Node host
  |
Pi-style TypeScript extension
```

Priority extension APIs:

1. tool registration;
2. slash-command registration;
3. lifecycle event subscription;
4. context hooks;
5. selected UI facilities.

The Node host should not start unless a compatible extension requires it.

## 9. Native extensions

A Rust/WASM-native extension system may be added later.

It must not replace the TypeScript compatibility path prematurely.

Goals:

- native low-overhead extensions;
- strong typed API;
- optional sandboxing;
- no requirement that Pi packages migrate.

## 10. Sessions

Session compatibility is expected to be partial initially.

Preferred architecture:

- maintain a stronger internal session/event model;
- provide Pi import/export adapters;
- preserve compatible message semantics;
- preserve model/tool metadata when representable;
- warn when `pi-rs`-specific provenance cannot round-trip.

Do not weaken the internal event model merely to force exact storage-format equivalence.

## 11. Themes and UI

Basic theme semantics may be supported.

Complex Pi custom UI behavior should be best-effort because:

- the TUI implementation differs;
- `pi-rs` adds new provenance and runtime states;
- exact widget-level parity may be costly.

Compatibility should focus first on preserving intent rather than exact rendering.

## 12. Provider compatibility

Provider compatibility is separate from Pi package compatibility.

The runtime should support:

- local OpenAI-compatible endpoints;
- remote OpenAI-compatible endpoints;
- provider-specific adapters where needed.

Provider normalization should preserve:

- model capability metadata;
- tool calls;
- exposed reasoning;
- stream completion;
- typed failures.

## 13. MCP compatibility

MCP version handling should remain isolated in the MCP adapter.

Requirements:

- protocol negotiation where practical;
- graceful support for selected older protocol versions;
- no assumption that every server supports the latest optional extensions;
- stable internal tool/resource normalization.

`rkb-rs` should be used as an early real-world compatibility fixture.

## 14. Compatibility tests

Maintain fixtures for:

```text
tests/compat/
  skills/
  prompts/
  packages/
  sessions/
  extensions/
```

Tests should cover both:

- synthetic minimal fixtures;
- representative real-world public packages.

## 15. Compatibility command

A future command should inspect an artifact or package:

```bash
pi-rs compat <path-or-package>
```

Possible output:

```text
Compatibility target: Pi <version-range>

✓ skill discovery
✓ prompt templates
✓ tool registration
△ context hook
✗ unsupported internal import
```

## 16. Version policy

`pi-rs` should declare which Pi behavior/version family it targets.

Compatibility changes should be documented in release notes.

When upstream Pi changes behavior:

1. detect via fixtures;
2. classify impact;
3. update compatibility implementation or document divergence;
4. avoid silent semantic drift.

## 17. Deliberate divergences

`pi-rs` may intentionally diverge where its runtime architecture requires stronger semantics.

Expected divergences include:

- richer event provenance;
- explicit model epochs;
- built-in context lifecycle;
- built-in failover;
- rehydratable external context;
- stricter tool transaction state;
- potentially different internal session storage.

These divergences should not unnecessarily break ecosystem-level reuse.

## 18. Compatibility principle

> Preserve user-facing ecosystem value before internal parity.

If supporting an undocumented internal behavior would significantly complicate the runtime, prefer a documented incompatibility over architectural debt.
