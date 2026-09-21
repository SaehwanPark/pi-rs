# Batch Relay

Batch Relay is a dependency-free Python service for accepting signed batches of
jobs and delivering runnable jobs to a local process. It uses SQLite for
durable state and deliberately provides a bounded `--once` worker rather than
a daemon or a message-broker protocol.

## Start the service

From this directory:

```text
python -m batchrelay serve --db state/relay.sqlite3 --secret development-secret --host 127.0.0.1 --port 8787
```

`POST /batches` requests must carry an `X-Batch-Signature` header in the form
`sha256=<lowercase HMAC-SHA256 hex digest>`. The digest covers the exact UTF-8
request body and uses the server secret as the UTF-8 key. The secret is never
stored in SQLite or returned by the API.

The service exposes `GET /healthz`, `POST /batches`, and
`GET /batches/<batch_id>`. A batch body has a non-empty `batch_id` and a
non-empty list of jobs. Each job has `job_id`, `kind`, `payload`, and
`depends_on`; ids are ASCII letters, digits, `.`, `_`, and `-`, payloads are
JSON objects, and dependencies must name jobs in the same acyclic batch.

The complete HTTP contract is in [SPEC.md](SPEC.md).

## Run the bounded worker

The worker invokes the sink with direct argv values and never starts a shell:

```text
python -m batchrelay worker --db state/relay.sqlite3 --sink python --sink-arg sink.py --sink-arg=--log --sink-arg=delivery.log --lease-seconds 30 --once
```

For each claimed job it sends one JSON line containing `batch_id`, `job_id`,
`kind`, `payload`, and `depends_on`. The sink returns one JSON object line with
the matching `job_id` and `ok: true` for success. `ok: false` may set
`retryable: false` for a terminal failure; otherwise the attempt remains
pending. Malformed output, an unexpected EOF, a sink error, and a retryable
rejection also leave the job pending and make that worker exit non-zero. One
`--once` invocation attempts each job at most once; a retryable job is deferred
to a later invocation rather than retried in a loop.

Claims are committed before the sink starts. A killed worker leaves a `leased`
job until its lease expires; a later `--once` worker reclaims it. Jobs wait for
all dependencies to succeed. A failed or blocked dependency makes its
dependents `blocked`, and a batch is `failed` when any job is failed or blocked.
A batch is `succeeded` only when every job succeeds.

## Tests

Run the project suite from this directory:

```text
python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v
```

The independent fresh-process oracle is run from the case root:

```text
python -W error::ResourceWarning -m unittest discover -s acceptance -p "test_*.py" -v
```

Only Python standard-library modules are used.
