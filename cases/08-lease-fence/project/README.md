# Lease Fence

Lease Fence is a dependency-free Python 3 service for accepting a signed,
ordered pipeline and delivering runnable jobs to a local process. It uses only
the Python standard library and SQLite.

## Run it

From this directory:

```text
python -m leasefence serve --db state/pipeline.sqlite3 --secret oracle-secret --host 127.0.0.1 --port 8787
python -m leasefence worker --db state/pipeline.sqlite3 --sink C:\\path\\to\\sink.exe --sink-arg=--mode --sink-arg=local --lease-seconds 30 --once
```

The worker invokes the sink with direct argv, never through a shell. Repeat
`--sink-arg=VALUE` for each sink argument. The top-level, `serve`, and
`worker` commands all expose `--help`.

## HTTP and signing

`GET /healthz` returns `{"ok": true}`. `POST /pipelines` accepts the exact
JSON body documented in `SPEC.md` and requires:

```text
X-Pipeline-Signature: sha256=<lowercase HMAC-SHA256 hex digest>
```

The digest covers the exact UTF-8 body and uses the server secret as its UTF-8
key. Invalid signatures return `401` and cannot create rows. Valid pipelines
are admitted atomically. Repeating identical content is `200` idempotency;
reusing the id with different content is `409`.

`GET /pipelines/<id>` returns the durable pipeline projection. It includes
`pending`, `leased`, `succeeded`, `failed`, and `blocked` job states, attempts,
errors, lease expiry, and outputs. The secret and private fencing token never
appear in responses.

## Dependencies and delivery

Jobs name direct dependencies in `depends_on`. `input_refs` explicitly copies
one top-level output field from a successful dependency. A `barrier` job has a
`collect` declaration and receives an ordered `fan_in.items` list containing
only the selected field from each successful direct dependency. A missing field
fails the barrier locally and blocks downstream jobs.

The sink receives one JSON line with `pipeline_id`, `job_id`, `kind`, `payload`,
`depends_on`, `inputs`, and `fan_in`. It must return one JSON line with matching
`job_id`, `ok: true`, and an object `output`. A retryable failure leaves the
job pending for a later worker invocation; a non-retryable failure marks it
failed and blocks dependents. A successful job is not sent again.

## Lease fencing

Each claim increments `attempts`, sets an expiry, and stores a private random
claim token. Finalization is conditional on both `status == leased` and the
same token. When a lease expires, a later worker clears the old token and
claims a new one. If the old worker returns after that, its result is rejected
as a stale lease, the worker exits non-zero, and the current job status/output/
error are unchanged. This prevents stale completion from overwriting the newer
winner; it does not promise exactly-once external side effects.

The worker is bounded: it makes one pass, attempts each job at most once per
invocation, and does not poll, use a shell, or make network calls. A lease is
1–300 seconds and an expired claim can be reclaimed by a fresh process.

## Checks

```text
python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v
```

The independent fresh-process oracle is one directory above the project:

```text
python -W error::ResourceWarning -m unittest discover -s ../acceptance -p "test_*.py" -v
```

The oracle starts separate service, worker, and sink processes and checks
authentication, atomic admission, ordered fan-out/barrier fan-in, declared
data flow, retries, blocked dependents, restart persistence, lease reclaim,
and stale-worker fencing. It does not import this package.
