# rupi Documentation Index

Welcome to the `rupi` documentation. This directory and repository contain the
specifications, architectural contracts, user guides, and historical development
records for `rupi`—a small, observable coding harness with selected Pi compatibility.
The current public release is **v0.2.2 (2026-09-20)**.

---

## 1. Public-Facing Documentation & Guides

For end users and system integrators, the primary documentation is published via
**GitHub Pages** (built with mdBook; deployment is validated from `book/` on pushes to
`main`):

- **Online Documentation:** [https://saehwanpark.github.io/rupi/](https://saehwanpark.github.io/rupi/)
- **Source Files:** [`book/src/`](../book/src/)
- **Topics Covered:**
  - **Quickstart Guide:** Get running with local or remote models in under 60 seconds.
  - **Installation:** checksum-verified shell/PowerShell installers, Cargo source builds,
    and prebuilt binaries.
  - **Test Ledger:** a live first-project walkthrough with independent verification,
    trace, and replay.
  - **Read Queue:** a live HTTP/SQLite project with restart recovery and a
      fresh-process acceptance oracle.
  - **Event Outbox:** a live HTTP/SQLite outbox with idempotency, retry state,
      a separate sink process, and an independent acceptance oracle.
  - **Webhook Inbox:** a live HMAC-authenticated HTTP/SQLite inbox with
      expiring leases, crash reclaim, a direct-argv sink, and an independent
      acceptance oracle.
  - **Batch Relay:** a live authenticated HTTP/SQLite batch worker with
      dependency DAGs, retry/blocked states, lease reclaim, and an independent
      acceptance oracle.
  - **CLI Reference:** Detailed usage of `run`, `interactive`, `trace`, `replay`, `skills`, `prompts`, `packages`, `trust`, `compat`, `import-pi`, and `export`.
  - **Interactive TUI:** Keyboard shortcuts, editor buffer, syntax highlighting, status line.
  - **Provenance Model:** Understanding `[native reasoning]`, `[provider summary]`, `[declared]`, and `[reconstructed]`.
  - **Failover & Recovery:** Primary/backup model configuration and capability matching.
  - **Tool Sandbox & Security:** Workspace confinement, auto-approvals, mutating tool protection.
  - **Model Context Protocol (MCP):** Lazy stdio client integration.
  - **Pi Ecosystem Compatibility:** Running Pi skills, prompts, packages, and importing/exporting sessions.

---

## 2. Core Specifications & Architectural Authorities

These documents serve as the authoritative sources of truth for the codebase:

| Document | Purpose |
| :--- | :--- |
| [`docs/PROJECT_DESIGN_CANONICAL.md`](PROJECT_DESIGN_CANONICAL.md) | **Canonical design authority.** Explains system thesis, core contracts, lifecycle, and component invariants. |
| [`ARCHITECTURE.md`](../ARCHITECTURE.md) | Runtime boundaries, subsystem contracts, crate breakdown, and concurrency invariants. |
| [`COMPATIBILITY.md`](../COMPATIBILITY.md) | Upstream Pi behavioral compatibility targets, coverage status, and regression fixtures. |
| [`docs/SCHEMA_REFERENCE.md`](SCHEMA_REFERENCE.md) | Current v0.2.2 reference for event payloads, session logs, and serialized structures; source remains authoritative. |
| [`docs/SESSION_COMPATIBILITY.md`](SESSION_COMPATIBILITY.md) | Bidirectional mapping between Pi session JSONL and rupi store schemas. |
| [`docs/IMPLEMENTATION_STATUS.md`](IMPLEMENTATION_STATUS.md) | Implementation inventory, verified capabilities, and phase deliverables. |

---

## 3. Developer & Contribution Guides

| Document | Purpose |
| :--- | :--- |
| [`CONTRIBUTING.md`](../CONTRIBUTING.md) | Verification workflows (`fmt`, `clippy`, `test`, `bench`), PR slicing, and coding conventions. |
| [`ROADMAP.md`](../ROADMAP.md) | Staged milestone roadmap and active tracking of project phases. |
| [`AGENTS.md`](../AGENTS.md) | Operational guidelines, invariant requirements, and usage policies for autonomous coding agents. |
| [`LESSONS.md`](../LESSONS.md) | Durable architectural lessons, edge cases encountered, and rationale for key design choices. |
| [`docs/codexbar.md`](codexbar.md) | Subscription quota monitoring rules and loop boundary policies. |
| [`docs/subagents_policy.md`](subagents_policy.md) | Subagent delegation topologies, memory conservation, and handoff protocols. |
| [`docs/harness/rupi-development/team-spec.md`](harness/rupi-development/team-spec.md) | Delivery harness specification and specialist agent roles. |

---

## 4. Historical Archive (`docs/archive/`)

Historical development artifacts from early bootstrapping and intermediate phases
are preserved for provenance and auditing:

- [`docs/archive/slices/`](archive/slices/): Individual slice specifications and
  validation contracts from Phases 1 through 11 (e.g., `SLICE_CLI.md`,
  `SLICE_EPOCH.md`, `SLICE_STATUSLINE.md`, `SLICE_WARM_START.md`).
- [`docs/archive/handoffs/`](archive/handoffs/): Milestone handoff reports from
  prior development sessions.
- [`docs/archive/proposals/`](archive/proposals/): Initial project proposal,
  early MVP validation reports, and compaction event audit records.
- [`audits/`](../audits/): Completed audit rounds, explicitly marked as historical and
  retained for provenance; they are not current implementation instructions.

The former root-level [`HANDOFF.md`](../HANDOFF.md) is now a short pointer. Its resolved
historical record lives in [`docs/archive/handoffs/`](archive/handoffs/).
