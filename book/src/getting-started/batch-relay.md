# Live example: Batch Relay

**Batch Relay** is the fifth runnable case in this repository. It keeps the
HMAC admission, SQLite persistence, leases, crash reclaim, and direct-argv sink
from [Webhook Inbox](webhook-inbox.md), then adds atomic dependency-DAG
admission, dependency-ordered scheduling, retryable failures, terminal
failures, and blocked dependents.

The [specification](https://github.com/SaehwanPark/rupi/blob/main/docs/cases/2026-09-20-batch-relay/project/SPEC.md),
[implementation](https://github.com/SaehwanPark/rupi/tree/main/docs/cases/2026-09-20-batch-relay/project),
and [independent acceptance oracle](https://github.com/SaehwanPark/rupi/blob/main/docs/cases/2026-09-20-batch-relay/acceptance/test_batchrelay.py)
are checked into the repository.

The checked-in project suite (9 tests) and fresh-process oracle (4 tests)
pass after bounded tester repair. The final local-model turns timed out before
their first project write; the [case evidence](https://github.com/SaehwanPark/rupi/tree/main/docs/cases/2026-09-20-batch-relay)
keeps that incomplete model result separate from the green fixture.

## 1. Verify the project independently

From the project directory, run its suite and inspect the command contracts:

```bash
cd docs/cases/2026-09-20-batch-relay/project
python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v
python -m batchrelay --help
python -m batchrelay serve --help
python -m batchrelay worker --help
```

Run the independent oracle from the case root:

```bash
cd ..
python -W error::ResourceWarning -m unittest discover -s acceptance -p "test_*.py" -v
cd project
```

The oracle starts fresh service, worker, and sink processes. It checks exact
HMAC admission, atomic DAG validation, idempotency and conflicts, dependency
ordering, retryable and terminal failures, blocked dependents, lease reclaim,
and restart persistence.

## 2. Run the service and worker

Start the service in one terminal:

```bash
python -m batchrelay serve --db state/relay.sqlite3 --secret development-secret --host 127.0.0.1 --port 8787
```

`POST /batches` accepts a batch with jobs. Each job has a unique `job_id`, a
`kind`, a JSON-object `payload`, and `depends_on` job ids. The
`X-Batch-Signature` header is `sha256=<hex digest>`, where the digest is the
HMAC-SHA256 of the exact raw UTF-8 request body using the server secret as the
UTF-8 key. Cycles, missing dependencies, invalid signatures, and malformed
requests do not create partial state. Repeating the same batch is idempotent;
changing its content returns a conflict.

Process available jobs once using the independent sink:

```bash
python -m batchrelay worker --db state/relay.sqlite3 \
  --sink python --sink-arg ../acceptance/fake_sink.py \
  --sink-arg=--log --sink-arg=state/delivery.ndjson \
  --lease-seconds 30 --once
```

The worker claims only jobs whose dependencies succeeded and commits each lease
before starting the sink. A retryable failure remains pending for a later
`--once` invocation. A terminal failure blocks dependents. A killed worker
leaves a lease that a later worker can reclaim after expiry. Sink arguments are
passed directly as argv, never through a shell, and the sink exchanges one JSON
line in each direction.

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

- exact-byte HMAC admission for a whole dependency DAG;
- atomic validation and idempotent batch persistence;
- dependency-aware scheduling with retry and blocked states;
- lease expiry and at-least-once direct-argv delivery; and
- trace/replay evidence for bounded or incomplete model turns.
