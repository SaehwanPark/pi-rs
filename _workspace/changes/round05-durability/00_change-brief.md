# Round 5 durability and secret-boundary remediation

## Request
Address `audits/20260919/round05.md` on top of `main`.

## Scope
Implement the five P1 findings as one bounded reliability/security slice:

1. Recover only ordinary torn/unterminated final JSONL tails before append, while
   continuing to fail closed on interior malformed records.
2. Publish content-addressed blobs durably before returning references, verify rename
   races, and make temporary names unique per write.
3. Make incomplete L1/L2 compactions and L3 checkpoint boundaries deterministic on
   resume instead of refusing an otherwise recoverable session.
4. Keep the kernel lease format marker immutable and make legacy migration explicit,
   avoiding the crash-poisoned empty-marker state.
5. Make literal credentials structurally non-serializable and redacted in `Debug`,
   including nested runtime/provider configuration; reject credential-bearing URLs.

The directly related blob temp-name and provider URL-userinfo P2 recommendations are
included. Other P2 performance/cleanup/schema-migration recommendations remain explicit
follow-ups unless implementation exposes a small, safe correction in the same paths.

## Acceptance criteria

- Focused regression tests exercise each failpoint/recovery rule and secret boundary.
- Interior malformed JSONL still fails closed; valid unterminated final records are
  normalized; invalid final tails are discarded safely.
- Blob references are not returned before temp/directory durability and collision
  contents are verified.
- Resuming after every durable append in representative compaction/checkpoint sequences
  yields either the exact prior context or the exact committed context.
- Lease acquisition cannot reinterpret an empty new-format marker as a legacy lease.
- `serde_json` and all relevant `Debug` output contain no literal credentials; URL
  userinfo is rejected.
- Repository checks pass, with failures attributable only if documented.

## Non-goals

- No broad WAL redesign or new external dependencies.
- No automatic replay of unknown mutating operations.
- No weakening of fail-closed behavior for contradictory or interior corruption.
- No changes to startup architecture beyond the persistence/security fixes.
