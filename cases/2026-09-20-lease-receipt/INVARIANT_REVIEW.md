# Invariant Review

Change: 2026-09-20-lease-receipt
Owner: fresh-memory tester
Status: ready
Inputs: `CASE_PLAN.md`, `project/SPEC.md`, project implementation/tests, independent oracle, trace/replay output

Verdict: pass

## Review passes

### Pass 1 — Contract and data flow

- Read the complete case specification and all case-local implementation paths.
- Confirmed atomic admission, deterministic ordering, dependency checks, declared
  input resolution, barrier-only fan-in, and restart-visible output state.
- Confirmed `delivery_key` is derived only from validated immutable ids and is
  separate from the private claim token.
- Confirmed accepted success persists output and receipt together; an EOF or
  malformed response cannot fabricate either.
- No actionable issue found.

### Pass 2 — Side effects, fencing, and resource ownership

- Confirmed claims increment attempts and store an unguessable private token;
  every finalization requires both the leased state and exact token.
- Confirmed a stale finalization reports failure and changes no newer row,
  including output, receipt, lease, attempts, or error.
- Confirmed sink invocation is direct argv, bounded, and never shell-mediated.
- Confirmed the independent oracle exercises reclaim/stale completion and a
  fresh-process lost-acknowledgement replay with one logical sink application.
- The first repeated oracle run exposed orphaned blocking sink helpers. The
  oracle-only cleanup was bounded to `acceptance/fake_sink.py` and
  `acceptance/test_lease_receipt.py`; the follow-up run exited 0 with zero
  case-local sink processes remaining.

### Pass 3 — Runtime boundaries and operational evidence

- Confirmed the branch diff contains only the case directory; no rupi source,
  repository tests, canonical docs, roadmap, README, or parent integration file
  changed.
- Confirmed model traces contain one local model, native provenance, no failover,
  and no unknown tool completion; `rupi trace` and `rupi replay` both exited 0
  without generating work.
- Confirmed startup benchmark remains well below the repository budget and all
  required workspace checks pass.
- No actionable issue found.

## Findings

No actionable issues found.

## Residual risk

The receipt rule is an explicit idempotent-sink protocol, not a general exactly-
once guarantee. A sink that ignores `delivery_key` can still observe duplicate
requests. Schema migration, multi-database coordination, and arbitrary
concurrent-worker guarantees remain outside this bounded case.
