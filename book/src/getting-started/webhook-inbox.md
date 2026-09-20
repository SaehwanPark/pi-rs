# Live example: Webhook Inbox

**Webhook Inbox** is the fourth runnable case in this repository. It extends
[Event Outbox](event-outbox.md) with HMAC-authenticated admission, SQLite
delivery leases, crash recovery, and a direct-argv sink worker. It uses only
the Python standard library.

The [specification](https://github.com/SaehwanPark/rupi/blob/main/docs/cases/2026-09-20-webhook-inbox/project/SPEC.md),
[implementation](https://github.com/SaehwanPark/rupi/tree/main/docs/cases/2026-09-20-webhook-inbox/project),
and [independent acceptance oracle](https://github.com/SaehwanPark/rupi/blob/main/docs/cases/2026-09-20-webhook-inbox/acceptance/test_webhook_inbox.py)
are checked into the repository.

The checked-in project suite (11 tests) and fresh-process oracle (2 tests)
pass. The final local-model authoring attempt did not complete a fresh copy;
that distinction is preserved in the [case reports](https://github.com/SaehwanPark/rupi/tree/main/docs/cases/2026-09-20-webhook-inbox)
and is not counted as model-completed implementation evidence.

## 1. Verify the project independently

From a clone of rupi, run the project tests and inspect its command contracts:

```bash
cd docs/cases/2026-09-20-webhook-inbox/project
python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v
python -m webhookinbox --help
python -m webhookinbox serve --help
python -m webhookinbox worker --help
```

Run the independent oracle from the case root, not from inside the project
directory:

```bash
cd ..
python -W error::ResourceWarning -m unittest discover -s acceptance -p "test_*.py" -v
cd project
```

The oracle starts fresh service, worker, and sink processes. It checks signed
admission, idempotency and conflicts, invalid-input non-mutation, delivery,
lease expiry, worker crash/reclaim, and SQLite persistence across restart.

## 2. Run the service and worker

Start the service in one terminal:

```bash
python -m webhookinbox serve --db var/inbox.sqlite3 --secret oracle-secret --host 127.0.0.1 --port 8080
```

`POST /deliveries` accepts a JSON object containing `delivery_id`,
`event_type`, and `payload`. Its `X-Webhook-Signature` header must be
`sha256=<hex digest>`, where the digest is the HMAC-SHA256 of the exact raw
UTF-8 request body using the server secret as the UTF-8 key. Repeating the same
delivery is idempotent; changing its content returns a conflict. The secret is
not stored in SQLite or returned in JSON.

In another terminal, process available deliveries once using the independent
oracle sink:

```bash
python -m webhookinbox worker --db var/inbox.sqlite3 \
  --sink python --sink-arg ../acceptance/fake_sink.py \
  --sink-arg=--log --sink-arg=./var/sink.ndjson \
  --sink-arg=--tag --sink-arg=manual --lease-seconds 30 --once
```

The worker commits a lease and increments the attempt count before starting
the sink. A crash leaves the row leased until expiry; a later `--once` worker
can reclaim it. This is at-least-once delivery, so a sink may see a delivery
again around a crash. Sink arguments are passed directly as argv, never through
a shell, and the sink exchanges one JSON line in each direction.

## 3. Ask rupi for a bounded verification

Keep implementation and independent acceptance in separate turns. A timeout or
request-budget exhaustion is an incomplete turn, not evidence that the project
passed. For a read-only review, disable mutating approval in a copied config:

```bash
rupi run --config rupi.recovery.config.json --cwd . \
  --prompt "Read SPEC.md and inspect the implementation briefly. Run the project suite in one bounded slice, report exact results, and do not run the independent acceptance oracle."
```

The checked-in implementation config enables mutations for the original case
study. Use a disposable copy or a version-controlled branch before allowing
edits.

## 4. Inspect and replay the evidence

After the run, inspect the durable trace without contacting the model or
executing old tools:

```bash
rupi trace --config rupi.recovery.config.json --quiet --no-reasoning
rupi replay .rupi-state-recovery/sessions/<session-id>.trace.jsonl --tools --sequence
```

Only call a new implementation complete after both the project suite and the
independent fresh-process oracle pass. This case demonstrates:

- constant-time HMAC admission over the exact request bytes;
- idempotent SQLite admission with explicit conflict behavior;
- durable leases and reclaimable at-least-once delivery;
- direct-argv process isolation for a line-delimited sink protocol; and
- trace/replay evidence for bounded work and incomplete model turns.
