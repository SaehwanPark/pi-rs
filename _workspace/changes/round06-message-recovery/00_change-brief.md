# Round 06 durability recovery

## Scope

Address both P1 findings in `audits/20260919/round06.md`:

1. make model-visible messages recoverable as one durable event/message transaction;
2. reconcile assistant tool calls that were projected before `ToolRequested` was written.

## Observable acceptance

- A pending message transaction retains an exact, redacted recovery payload in a bounded inline/blob form.
- Reopening after any transaction boundary either restores the exact message or performs an explicit safe closure; ordinary runtime-generated states do not require manual repair.
- User, external-context, assistant text, assistant tool-call, successful/reduced tool result, failed tool result, and unknown tool result projections are covered by failpoint tests at prepare, canonical append, and semantic append boundaries.
- Assistant calls with no canonical `ToolRequested` are closed as never executed, without running or retrying tool code; already requested calls retain existing unstarted/started recovery semantics.
- Existing canonical trace, redaction, sequence, compaction/checkpoint, and uncertain-side-effect invariants remain intact.

## Planned vertical slices

1. Add a bounded message recovery payload/reference contract and atomic store API.
2. Route runtime message emission through that API and add deterministic interruption tests.
3. Reconcile projected assistant calls before projection-alignment validation, including multi-call partial progress.
4. Run invariant review and full repository verification; update architecture/roadmap only where the durable contract changed.

## Non-goals

- automatic retry of any tool call;
- replay of uncertain or started mutating operations;
- broad filesystem failpoint coverage beyond the message transaction seam;
- unrelated P2 audit findings.
