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
| Package installation | Partial |
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

A session offers its skills the way Pi does. `pi-rs run` and `pi-rs interactive` scan
the global locations at session open and put the result in front of every request as the
system message: Pi's skill-control block — the three instruction sentences, then one
`<skill>` entry per visible skill with its `name`, `description`, and `location` (the
file the read tool should be given), XML-escaped as the Agent Skills standard spells it.
A `disable-model-invocation` skill is absent from the block down to its name; asking for
it by name (`pi-rs skills --show <name>`, which prints the body with the frontmatter
removed) is the explicit invocation that flag reserves for the user.
`pi-rs skills --control-prompt` prints exactly what a session would send, empty stdout
when there is nothing to offer — and only global locations reach a session, because the
workspace's own skill files need a trust decision `run` does not have. Discovery commands
accept `--trust-store <dir>` to consult durable canonical project scopes; an explicit
`--project` is a one-shot grant for an unknown scope, but a recorded denial still wins.

Package-local skills from discovered packages and `--skill <path>` CLI options are supported. The `skills` array in settings is deferred.

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

`pi-rs interactive` invokes a loaded template as `/name [arguments…]`: the typed string
is split by Pi's editor rule (`parseCommandArgs`) — bash-style quotes whose quirks are
reproduced, not smoothed — and the expansion is sent as the turn.

Package `prompts/` directories, `pi.prompts` entries, `--prompt-template` paths,
and `--no-prompt-templates` are supported. The `prompts` array in settings is deferred.

## 7. Packages

Support Pi-style package discovery and installation as early as practical. Discovery and
surface diagnostics are supported, and `pi-rs packages install <local-directory>` now copies
an explicit local package into the global or `--project` package root without running
scripts. npm/git/HTTP sources, dependency installation, update/remove settings, and
extension execution remain deferred; the surface therefore stays `Partial`.

Goals:

- recognize compatible package manifests;
- install an explicit local package directory without following symlinks or running scripts;
- expose contained skills/prompts/extensions;
- report unsupported package surfaces clearly;
- leave remote source resolution and dependency installation explicit as unsupported until
  their subprocess/network/trust contract is implemented.

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

### 7.1 Implemented

`pi-rs packages [--project]` scans, in order: `$HOME/.pi/agent/packages`,
`$HOME/.pi/packages`, then `<ancestor>/.pi/packages` from the working directory up
to the git root. `--trust-store <dir>` can resolve the project scope before those
locations are read. Discovery is non-recursive at each package root: every child
directory holding a `package.json` is a package candidate. A package declares its
identity (`name`, `version`, `description`) and contained surfaces (`pi.skills`,
`pi.prompts`, `extensions`).

When a package declares no explicit skill or prompt paths, standard conventions apply:
`<package>/skills/` or `<package>/SKILL.md` is exposed for skills, and `<package>/prompts/`
for prompt templates. Project package locations are read only when the project is trusted
(`--project`), preserving the trust boundary. Manifest surface paths must be relative and
contained within the package; absolute, parent-traversing, or outward-symlink paths are
reported and not activated. Duplicate package names resolve to the first package found.
Unsupported surfaces (such as `extensions` requiring the Node host) produce per-surface
diagnostics without rejecting the package.

`pi-rs packages --show <name>` displays detailed surface status and contained locations.

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

The path may also be a directory: every `*.jsonl` directly inside it is imported, in name
order, each as its own session. That is also the boundary of what a batch carries — lineage
Pi records *across* files (a fork written as a second file pointing at the first) is not
reconstructed, because no pi-rs session state has that shape. One file's failure is one
report line; the sessions beside it still land, and the exit says the batch was partial.

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

### 10.2 Exporting a Pi session

`pi-rs export <session-id> [--out <path>]` writes one `pi-rs` session back out in Pi's JSONL shape.
The emitted shape is derived from what `pi-rs import-pi` accepts:

- A version 3 header (`type: "session"`, `id`, `cwd`, `timestamp`);
- Linear message entries linked sequentially with `parentId`;
- Assistant reply text and tool calls folded from streamed deltas into Pi message blocks.

Anything the canonical trace holds that Pi's shape cannot carry is explicitly surfaced on
stderr as a dropped item rather than silently discarded or falsified.

### 10.3 Non-round-trippable metadata

See [`docs/SESSION_COMPATIBILITY.md`](docs/SESSION_COMPATIBILITY.md) for the complete bidirectional
fidelity matrix and design invariants.

Summary of metadata that cannot round-trip between Pi and `pi-rs`:

1. **Reasoning provenance**: `pi-rs` models 4 distinct provenance tiers (`Native`, `ProviderSummary`,
   `DeclaredRationale`, `ReconstructedRationale`). Pi has untyped `thinking` blocks with no
   provenance concept. Export reports dropped provenance on stderr; import never invents native
   provenance for unlabelled foreign thinking.
2. **Multi-branch DAGs vs linear turns**: Pi records branching trees (`id`/`parentId`). `pi-rs`
   imports only the active path from the newest-written entry to root; off-path forks are dropped
   and reported by type on stderr.
3. **Model epochs & failovers**: `pi-rs` tracks typed model epochs (`ModelEpochStarted`) and
   reasons for failover. Pi stores only string `model` fields per assistant message.
4. **Tool lifecycle & safety invariants**: `pi-rs` tracks 5 execution states (`Requested`, `Started`,
   `Completed`, `Failed`, `Unknown`) and `read_only` safety flags in `trace.jsonl`. Pi records only
   coarse messages.
5. **Payload externalization & blob stores**: Payloads exceeding 8 KiB reside in `blobs/sha256/...`
   in `pi-rs`. Pi has no session blob store; export emits the inline preview.
6. **Context reductions & redactions**: `ContextReduced` token/byte statistics and durable redaction
   counts exist only in `pi-rs` canonical traces.
7. **Diagnostics & checkpoints**: Operational events (`Diagnostic`, `Checkpoint`, `SessionEnded`)
   have no counterpart in Pi message logs and stay in `pi-rs` traces.
8. **Compaction summaries**: Pi compaction summaries are marked as diagnostic boundaries on import,
   preventing duplicate turn replay.
9. **UI / Extension entries**: Pi `label`, `custom`, and `custom_message` records are dropped on
   import with explicit stderr warnings.
10. **Provider billing & cache usage**: Pi's `usage.cacheRead`, `usage.cacheWrite`, and `cost` are
    omitted since `pi-rs` tracks only token quantities.

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

MCP version handling should remain isolated in the MCP adapter. Stdio and the bounded
Streamable HTTP transport are supported; HTTP responses are JSON or matching-response SSE,
with session headers and bounded bodies. Long-lived server push and cancellation-aware HTTP
reads remain deferred.

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

Inspects an artifact or package candidate against Pi compatibility targets:

```bash
pi-rs compat [options] <path-or-package>
```

Options:
- `--project`: include project package locations when resolving package names;
- `--json`: emit machine-readable JSON report.

Example output:

```text
Compatibility target: Pi 0.50.x+
Target: with-extension (package)

✓ package manifest (with-extension@1.0.0)
✓ skill discovery (1 skill(s) found)
✓ prompt templates (1 prompt template(s) found)
✓ tool registration (registerTool API detected)
✓ command registration (registerCommand API detected)
△ context hook (context lifecycle hook detected)
✗ unsupported internal import (internal Pi module import detected)
✗ extensions (TypeScript host required in Phase 8)
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
