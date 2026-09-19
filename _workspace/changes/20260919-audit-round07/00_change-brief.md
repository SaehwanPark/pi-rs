# Round 7 audit fixes

## Request

Address `audits/20260919/round07.md` against `main` (`e41b99c`). Open a draft PR early, keep progress visible through incremental commits, and merge the completed, verified change into `main`.

## Vertical slice

Bring live execution, interactive lifecycle handling, and Windows interrupt behavior into agreement with the durable recovery contracts:

1. make tool invocation identity causal and reject duplicate provider call IDs within one assistant response while preserving older parentless traces;
2. durably close interactive sessions on explicit user exit and typed fatal turn failures, while leaving sink failures unclosed for recovery;
3. propagate management-command sink failures instead of continuing on an untrusted session handle;
4. implement Windows console Ctrl-C cancellation for in-flight turns;
5. address the bounded release-hardening findings from the same audit where practical (semantic-message preflight, panic cleanup, provider debug redaction, resolver cancellation, and orphan recovery-blob cleanup).

## Owned paths

- `crates/pi-rs-core/`, `crates/pi-rs-provider/`, `crates/pi-rs-runtime/`, `crates/pi-rs-store/`
- `src/interactive.rs`, `src/run.rs`
- relevant integration/unit tests and architecture/roadmap notes

## Non-goals

- no new provider orchestration or replay behavior;
- no weakening of `Unknown` tool semantics or sink-failure recovery barriers;
- no broad storage-format migration beyond backward-compatible causal-parent support;
- no unrelated UX redesign.

## Acceptance evidence

- regression tests cover duplicate IDs within a response, reused IDs across turns/model rounds, interactive clean/fatal/sink exits, management-command sink failures, and Windows interrupt handling;
- formatting, clippy, workspace tests, docs, and affected benchmarks pass;
- invariant review records no blocking findings;
- draft PR is updated with validation and residual-risk notes before merge.
