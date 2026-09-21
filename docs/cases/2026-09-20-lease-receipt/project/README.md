# Lease Receipt

Lease Receipt is a small Python 3 standard-library service for authenticated,
durable pipeline delivery. It uses SQLite for state, a bounded direct-argv
worker, and an explicit sink receipt contract.

## Run it

From this directory:

```text
python -m leasereceipt serve --db state/pipeline.sqlite3 --secret dev-secret --host 127.0.0.1 --port 8080
python -m leasereceipt worker --db state/pipeline.sqlite3 --sink python --sink-arg=path/to/sink.py --once
```

The worker's `--sink-arg` values are passed as argv values, never through a
shell. Use `--sink-arg=VALUE` when a value begins with `-`. The worker claims
each currently runnable job at most once and exits; it does not poll.

## HTTP and signing

`GET /healthz` returns `{"ok": true}`. `POST /pipelines` accepts the exact
pipeline JSON described in `SPEC.md` and requires
`X-Pipeline-Signature: sha256=<lowercase HMAC-SHA256>`, computed over the exact
UTF-8 request bytes with the configured secret. Invalid signatures are `401`
and do not mutate SQLite. `GET /pipelines/<pipeline_id>` returns the durable
pipeline and ordered jobs.

Admission is atomic. Repeating the same canonical pipeline is idempotent (`200`);
different content under an existing id is a conflict (`409`). Jobs are
`pending`, `leased`, `succeeded`, `failed`, or `blocked`. Retryable sink
failures leave a job pending for a later worker; terminal failures block
dependents.

## Leases, fencing, and receipts

Each claim increments `attempts`, sets an expiry, and stores a private random
claim token. Finalization requires the current token and a leased row. A stale
worker therefore cannot overwrite the newer claim after reclaim and reports a
non-zero stale outcome. The private token is never sent to a sink or returned
by HTTP.

Every job also has a stable public `delivery_key`,
`pipeline_id + ":" + job_id`. It is reused across retries and reclaim, so a
sink can persist a receipt and deduplicate a logical delivery. A successful
sink response must return matching `job_id`, matching `delivery_key`, an object
`output`, and a non-empty `receipt_id`. The receipt is stored with the job.

If a sink applies its side effect but the acknowledgement is lost, the worker
records a retryable failure rather than inventing success. A later worker sends
the same delivery key; an idempotent sink can return its stored output and
receipt without applying the logical side effect twice. This is an
at-least-once/idempotent-sink contract, not a general exactly-once guarantee.

## Dependencies and checks

Only Python standard-library modules are used. From `project/` run:

```text
python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v
```

The independent fresh-process oracle is run from the case root:

```text
python -W error::ResourceWarning -m unittest discover -s acceptance -p "test_*.py" -v
```
