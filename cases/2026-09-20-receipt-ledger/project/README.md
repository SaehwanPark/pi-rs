# Receipt Ledger

Receipt Ledger is a small Python 3 standard-library service for authenticated,
durable pipeline delivery. It uses SQLite, a bounded direct-argv worker, an
idempotent sink-receipt contract, and a tamper-evident operational audit ledger.

## Run it

From this directory:

```text
python -m receiptledger serve --db state/pipeline.sqlite3 --secret dev-secret --host 127.0.0.1 --port 8080
python -m receiptledger worker --db state/pipeline.sqlite3 --sink python --sink-arg=path/to/sink.py --once
python -m receiptledger audit --db state/pipeline.sqlite3 --verify
python -m receiptledger audit --db state/pipeline.sqlite3 --tail 10
```

The worker passes `--sink-arg` values directly as argv, never through a shell.
Use the `--sink-arg=VALUE` spelling when a value begins with `-`. Each
`--once` invocation claims each currently runnable job at most once and exits;
it never polls for new work.

## HTTP and signing

`GET /healthz` returns `{"ok": true}`. `POST /pipelines` accepts the exact
pipeline JSON described in `SPEC.md` and requires
`X-Pipeline-Signature: sha256=<lowercase HMAC-SHA256>`, computed over the exact
UTF-8 request bytes with the configured secret. Invalid signatures are `401`
and do not mutate SQLite. `GET /pipelines/<pipeline_id>` returns ordered,
durable job state.

Admission is atomic. The first canonical pipeline is `202`; the same content
under the same id is an idempotent `200`; different content is a `409`. Jobs
are `pending`, `leased`, `succeeded`, `failed`, or `blocked`. Retryable sink
failures leave a job pending for a later worker; terminal failures block
dependents. Dependencies and barrier fan-in are declared in the request, and
only declared output fields cross a job boundary.

## Leases, fencing, and receipts

Each claim increments `attempts`, sets an expiry, and stores a private random
claim token. Finalization requires the current token and a leased row, so a
stale worker cannot overwrite a newer claim. The private token is never sent
to a sink or returned by HTTP.

Each job also has a stable public `delivery_key`,
`pipeline_id + ":" + job_id`. It is reused across retries and reclaim. A
successful sink response must return matching `job_id`, matching
`delivery_key`, an object `output`, and a non-empty `receipt_id`.

If a sink applies its side effect but the acknowledgement is lost, the worker
does not invent success. The next worker sends the same delivery key; an
idempotent sink can replay its durable output and receipt without applying the
logical key twice. This is at-least-once delivery with an idempotent-sink
protocol, not exactly-once delivery for arbitrary programs.

## Audit ledger

Pipeline admission and every authoritative job transition append a safe event
to the SQLite `audit_events` table in the same transaction as the state change.
The event JSON is canonical (sorted keys and compact separators). Rows have a
contiguous `seq`, `prev_hash`, and `event_hash`:

```text
event_hash = SHA256(prev_hash + "\n" + event_json)
```

The first previous hash is 64 zeroes. Events contain bounded public ids,
status, attempts, and fixed outcome/detail labels. They never contain the HMAC
secret, private lease token, raw sink argv, or arbitrary sink bytes. A lost
acknowledgement remains an unknown outcome until a matching receipt is seen.

`audit --verify` opens the database read-only and recomputes every sequence and
hash. It never repairs, truncates, replays, or executes a sink. `audit --tail`
is a read-only event projection and is not a work queue.

## Checks

Only Python standard-library imports are used. From `project/`:

```text
python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v
```

The independent fresh-process oracle is run from the case root:

```text
python -W error::ResourceWarning -m unittest discover -s acceptance -p "test_*.py" -v
```
