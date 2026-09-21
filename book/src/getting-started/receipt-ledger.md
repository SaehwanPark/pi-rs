# Live example: Receipt Ledger

**Receipt Ledger** is the tenth runnable case in this repository. It keeps the
authenticated admission, SQLite state, dependency scheduling, ordered barrier
fan-in, private lease fencing, reclaim, stable delivery keys, sink receipts,
and direct-argv worker from [Lease Receipt](lease-receipt.md), then adds one
bounded operational-evidence dimension: an append-only, tamper-evident audit
ledger.

Authoritative admission and job transitions append canonical public events in
the same SQLite transaction as the state change. Each event is linked with a
SHA-256 hash chain. The audit ledger is read-only evidence; it is separate from
rupi's canonical trace and model-visible context, and it never controls work
or replays a sink.

The [specification](https://github.com/SaehwanPark/rupi/blob/main/docs/cases/2026-09-20-receipt-ledger/project/SPEC.md),
[implementation](https://github.com/SaehwanPark/rupi/tree/main/docs/cases/2026-09-20-receipt-ledger/project),
and [independent acceptance oracle](https://github.com/SaehwanPark/rupi/blob/main/docs/cases/2026-09-20-receipt-ledger/acceptance/test_receipt_ledger.py)
are checked into the repository.

The checked-in project suite (9 tests) and fresh-process oracle (4 tests) pass
after bounded tester repair. The local-model turns did not complete the
project; the [case evidence](https://github.com/SaehwanPark/rupi/tree/main/docs/cases/2026-09-20-receipt-ledger)
keeps that model stopgate separate from the green fixture.

## 1. Verify the project independently

From the project directory, run its suite and inspect the command contracts:

```bash
cd docs/cases/2026-09-20-receipt-ledger/project
python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v
python -m receiptledger --help
python -m receiptledger serve --help
python -m receiptledger worker --help
python -m receiptledger audit --help
```

Run the independent oracle from the case root:

```bash
cd ..
python -W error::ResourceWarning -m unittest discover -s acceptance -p "test_*.py" -v
cd project
```

The oracle starts fresh service, worker, sink, audit, and tamper-check
processes. It checks authenticated atomic admission, idempotency and
conflicts, ordered fan-out and barrier fan-in, declared output references,
retryable and terminal failures, blocked dependents, restart persistence, lease
reclaim, stale-worker fencing, lost-acknowledgement receipt replay, audit
verification, tamper detection, direct-argv execution, help output, and the
standard-library-only boundary.

## 2. Run the service, worker, and audit verifier

Start the service in one terminal:

```bash
python -m receiptledger serve --db state/pipeline.sqlite3 \
  --secret development-secret --host 127.0.0.1 --port 8787
```

`POST /pipelines` accepts the signed pipeline contract from `SPEC.md`.
`input_refs` copy only explicitly named top-level output fields, and a
`barrier` collects one selected field from each direct dependency in declared
order. Every job receives a deterministic `delivery_key` of
`pipeline_id:job_id`; it contains no secret and is stable across retries,
reclaims, and restarts.

Process available jobs once using a receipt-aware sink:

```bash
python -m receiptledger worker --db state/pipeline.sqlite3 \
  --sink python --sink-arg ../acceptance/fake_sink.py \
  --sink-arg=--log --sink-arg=state/delivery.ndjson \
  --lease-seconds 30 --once
```

Verify the audit chain or view a bounded safe tail:

```bash
python -m receiptledger audit --db state/pipeline.sqlite3 --verify
python -m receiptledger audit --db state/pipeline.sqlite3 --tail 10
```

Audit rows contain only bounded public identifiers, status, attempts, and fixed
outcome/detail labels. They never contain the HMAC secret, private lease token,
raw sink argv, or arbitrary sink bytes. `audit --verify` opens SQLite read-only
and recomputes contiguous sequence numbers and
`SHA256(prev_hash + "\\n" + canonical_event_json)`. It never repairs,
truncates, replays, or executes a sink.

The audit ledger does not make delivery exactly once. A lost sink
acknowledgement remains uncertain until a matching durable receipt is observed;
the next worker reuses the stable delivery key under the at-least-once,
idempotent-sink contract.

## 3. Ask rupi for a bounded verification

Keep implementation and independent acceptance in separate turns. A timeout or
request-budget exhaustion is incomplete work, not evidence of a passing
project. For a read-only review, use the separate verification config and ask
the model to run only the project suite:

```bash
rupi run --config rupi.verify.config.json --cwd . \
  --prompt "Read SPEC.md and inspect the implementation briefly. Run the project suite in one bounded slice, report exact results, and do not run the independent acceptance oracle."
```

Use a disposable copy or version-controlled branch before enabling mutations
with the implementation config.

## 4. Inspect and replay the evidence

After a run, inspect the durable rupi trace without contacting the model or
executing old tools:

```bash
rupi trace --config rupi.verify.config.json --quiet --no-reasoning
rupi replay .rupi-state-verify/sessions/<session-id>.trace.jsonl --tools --sequence
```

Only call a new implementation complete after both the project suite and the
independent fresh-process oracle pass. This case demonstrates:

- same-transaction operational audit events with a canonical SHA-256 chain;
- independent read-only verification and tamper detection;
- safe-field audit projection separate from side-effect authority and rupi
  provenance;
- stable delivery keys, receipts, fencing, retry/blocked state, and lease
  reclaim; and
- read-only trace/replay evidence for bounded model work.
