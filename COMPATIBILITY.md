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
locations are read only with `--project`. Walks are depth-bounded, and symlinks are
followed — linking in a skill kept elsewhere is the normal way to share one, and trust was
already decided about the directory holding the link — with the depth bound stopping a
link that walks a scan back on itself and saying so.

Not yet: package-local skills, the `skills` array in settings, `--skill` paths,
skill-body activation, and the model-facing prompt listing.

## 6. Prompt templates

Prompt templates should preserve:

- naming;
- discovery;
- invocation;
- expected variable behavior where documented.

Prompt compatibility should remain independent from the runtime provider implementation.

### 6.1 Implemented

`pi-rs prompts [--project]` scans, in order, `$HOME/.pi/agent/prompts/*.md` then
`<ancestor>/.pi/prompts/*.md` from the working directory up to the git root — the
locations Pi documents, in the order it reads them, so first-found-wins naming is
reproducible. Discovery is non-recursive and matches `*.md`, which is why a
subdirectory, a `.txt`, and a dotfile are skipped without a word: Pi documents those
rules and skips them silently, so a warning on every run would only train the user to
ignore warnings. A template's name is its filename without the extension; Pi imposes no
spelling rule on one, so neither does this. Frontmatter is optional in full: the
`description` is read when declared, otherwise Pi takes the first non-empty body line and
this says so in the listing rather than presenting an unauthored line as a summary. The
body is kept with only its surrounding blank lines removed — line breaks inside a template
are part of the prompt.

`pi-rs prompt [--project] <name> [arguments…]` performs Pi's substitution and writes the
prompt to stdout and nothing else: `$1`…`$n`, `$@` and `$ARGUMENTS`, `${1:-default}`,
`${@:-default}`, `${ARGUMENTS:-default}`, `${@:N}`, `${@:N:L}`. Since `$1` is a
placeholder, so is every digit after a `$`; a placeholder that matches none of the grammar
is left in the output exactly as written rather than deleted or rejected. Options are read
before the name, so `pi-rs prompt lint --strict` passes `--strict` to the template.

Not yet: invoking `/name` inside a running session, splitting one typed string into
arguments the way the editor does, package `prompts/` directories, `pi.prompts` entries,
the `prompts` array in settings, `--prompt-template` paths, and `--no-prompt-templates`.

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

### 10.1 Importing a Pi session

`pi-rs import-pi <session.jsonl>` reads one Pi session file and files it as a pi-rs
session. It is a reader and a writer, never an executor: a tool call in Pi's file records
work Pi already did, and `pi-rs` will not do it again.

Mapped, at the fidelity the file supports:

| Pi | `pi-rs` |
| --- | --- |
| `message` (role `user`) | `user_message`, with non-text blocks counted as attachments |
| `message` (role `assistant`) | one request span: `reasoning_delta`, `assistant_delta`, `tool_requested`, `model_request_completed` carrying Pi's usage and stop reason |
| `message` (role `toolResult`) | `tool_completed`, output filed past the store's inline threshold, under the durable redaction policy |
| `model_change`, `thinking_level_change` | info `diagnostic`; opening a model epoch would claim capabilities Pi never recorded |
| entry tree (`id` / `parentId`) | the path from the newest-written entry to the root; everything else is counted per type and named |
| header line | session id `pi-<pi id>`, `imported_from: "pi"`, Pi's `cwd`, the first readable entry timestamp |

One plan writes two durable records. The trace journal holds the events above. The session
log holds the conversation as message records, each bound to the event that introduced it: a
user message to its `user_message`, an assistant reply — its prose and its calls, never its
reasoning — to the first `assistant_delta`, a tool result to its terminal tool event. That
binding is what makes an imported session resumable rather than merely readable, and it is
the same mechanism a native session uses, so `load_session` needs no idea that Pi was
involved. Pi records no turn ids, so the import anchors one turn per user entry (`turn-<entry
id>`); it is derived from the file, so re-importing produces the same turns.

Deliberately not carried, each reported by kind with its reason:

* `compaction` — the boundary is kept as a diagnostic; re-importing the summary text would
  put the conversation in twice;
* `label`, `custom`, `custom_message` — UI- or extension-owned content with no `pi-rs`
  event, which importing as prose would misattribute to the user or the model;
* `usage.cacheRead` / `cacheWrite` / `cost` — `pi-rs` events have no field for them;
* an image whose Pi entry carried no bytes — counted as an attachment and reported. An image
  that did carry bytes is kept inline in the session's messages, where `pi-rs` keeps one;
* reasoning on a request that stored none receives **no** provenance rather than a plausible one.

Timestamps are parsed without a date library. `Z` and `±HH:MM` are honoured; a zone-less
stamp is read as UTC. A consistently wrong reading beats discarding the stamp: entries in
one file keep their relative order either way, and Pi writes `Z` in practice.

Silence is only for what Pi deliberately ignores. Everything else — a damaged line, an
unrecognised entry type, an entry off the active path — is reported with the path and the
reason, and a missing header, duplicate id, dangling parent, or cycle is an error rather
than something to paper over.

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
