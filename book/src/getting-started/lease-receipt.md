# Live example: Lease Receipt

**Lease Receipt** is the ninth runnable case in this repository. It keeps the
authenticated admission, SQLite state, dependency scheduling, ordered barrier
fan-in, private lease fencing, reclaim, retry handling, and direct-argv sink
from [Lease Fence](lease-fence.md), then adds a bounded lost-acknowledgement
recovery contract.

Each job has a stable public `delivery_key`, while each claim still has a
private fencing token. A sink can persist a `receipt_id` for that delivery
key, so a fresh worker can replay an uncertain request without applying the
logical sink effect twice. This is at-least-once delivery with an
idempotent-sink protocol, not a general exactly-once guarantee.

The [specification](https://github.com/SaehwanPark/rupi/blob/main/docs/cases/2026-09-20-lease-receipt/project/SPEC.md),
[implementation](https://github.com/SaehwanPark/rupi/tree/main/docs/cases/2026-09-20-lease-receipt/project),
and [independent acceptance oracle](https://github.com/SaehwanPark/rupi/blob/main/docs/cases/2026-09-20-lease-receipt/acceptance/test_lease_receipt.py)
are checked into the repository.

The checked-in project suite (8 tests) and fresh-process oracle (5 tests) pass
after bounded tester repair. The local-model turns did not complete the
project; the [case evidence](https://github.com/SaehwanPark/rupi/tree/main/docs/cases/2026-09-20-lease-receipt)
keeps that model stopgate separate from the green fixture.

## 1. Verify the project independently

From the project directory, run its suite and inspect the command contracts:

```bash
cd docs/cases/2026-09-20-lease-receipt/project
python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v
python -m leasereceipt --help
python -m leasereceipt serve --help
python -m leasereceipt worker --help
```

Run the independent oracle from the case root:

```bash
cd ..
python -W error::ResourceWarning -m unittest discover -s acceptance -p "test_*.py" -v
cd project
```

The oracle starts separate service, worker, and sink processes. It checks
authenticated atomic admission, idempotency and conflicts, ordered fan-out and
barrier fan-in, declared output references, retryable and terminal failures,
blocked dependents, restart persistence, lease reclaim, stale-worker fencing,
lost-acknowledgement receipt replay, direct-argv execution, help output, and
the standard-library-only boundary.

## 2. Run the service and worker

Start the service in one terminal:

```bash
python -m leasereceipt serve --db state/pipeline.sqlite3 \
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
python -m leasereceipt worker --db state/pipeline.sqlite3 \
  --sink python --sink-arg ../acceptance/fake_sink.py \
  --sink-arg=--log --sink-arg=state/delivery.ndjson \
  --lease-seconds 30 --once
```

The sink must return a matching `job_id`, `delivery_key`, object-valued
`output`, and non-empty `receipt_id`. If the sink applies its side effect but
the acknowledgement is lost, the worker records an uncertain retryable
failure. After lease reclaim, an idempotent sink receives the same delivery
key and can return its stored receipt/output without applying the logical
effect again. Private claim tokens remain hidden and stale finalization is
rejected.

Retryable failures remain pending for a later worker, terminal failures block
dependents, and a killed worker can be reclaimed after expiry. The worker is
bounded, does not poll, and never invokes the sink through a shell.

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

After a run, inspect the durable trace without contacting the model or
executing old tools:

```bash
rupi trace --config rupi.verify.config.json --quiet --no-reasoning
rupi replay .rupi-state-verify/sessions/<session-id>.trace.jsonl --tools --sequence
```

Only call a new implementation complete after both the project suite and the
independent fresh-process oracle pass. This case demonstrates:

- stable logical delivery identity separate from private claim identity;
- durable receipt replay after a lost acknowledgement or worker crash;
- explicit at-least-once/idempotent-sink semantics without an exactly-once
  claim;
- ordered barrier fan-in, retry/blocked state, lease reclaim, and fencing; and
- read-only trace/replay evidence for bounded model work.
