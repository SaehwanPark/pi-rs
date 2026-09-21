# Lease Cascade

Lease Cascade is a dependency-free Python 3 reference project for the Loop 6
live case. It accepts signed pipelines over HTTP, stores them in SQLite, and
delivers runnable jobs to a local sink through a bounded direct-argv worker.

The project adds one small data-flow rule beyond Artifact Pipeline: a `barrier`
job can collect one explicitly selected top-level output field from each
successful direct dependency. The collection is ordered by `depends_on`; a
missing selected field fails the barrier locally and blocks its dependents.

## Run

From this directory, using only the Python standard library:

```text
python -m leasecascade serve --db state/pipeline.sqlite3 --secret demo --host 127.0.0.1 --port 8123
python -m leasecascade worker --db state/pipeline.sqlite3 --sink python --sink-arg sink.py --lease-seconds 30 --once
```

The worker requires `--once`, invokes the sink with a direct argument vector,
and exits after the current runnable jobs. It does not poll. `--sink-arg` is
repeatable and preserves values beginning with `-` as sink data.

## HTTP and signatures

`GET /healthz` returns `{"ok": true}`. `POST /pipelines` accepts the exact JSON
document described in `SPEC.md`. Send the exact UTF-8 request body with:

```text
X-Pipeline-Signature: sha256=<lowercase HMAC-SHA256 hex digest>
```

The digest uses the server secret as its UTF-8 key. Invalid, missing, or
malformed signatures return `401` before JSON validation or database mutation.
`GET /pipelines/<pipeline_id>` returns the durable pipeline and job state.

## Job states and data flow

Jobs start `pending`, become `leased` when claimed, and finish `succeeded`,
`failed`, or `blocked`. A lease expiry returns a job to `pending` for a fresh
worker. A retryable sink response leaves the job pending for a later invocation;
a terminal response fails it. Any dependent of a failed or blocked job becomes
blocked and is never sent.

Every job has `input_refs` whose values name one top-level field from a
successful declared dependency. Only those values enter the sink request under
`inputs`; private or undeclared output keys do not.

A barrier job has `kind: "barrier"` and a `collect` object such as:

```json
{"field":"artifact_id","as":"artifacts"}
```

Its sink request contains:

```json
"fan_in": {
  "as": "artifacts",
  "items": [
    {"job_id":"part-a","value":"artifact-a"},
    {"job_id":"part-b","value":"artifact-b"}
  ]
}
```

The items contain only the selected field, in the declared dependency order.
The barrier is not sent until all direct dependencies succeed. If a selected
field is absent, the barrier is marked failed without a sink attempt and its
downstream jobs are blocked.

## Sink protocol and persistence

For each claimed job the worker writes one JSON object plus a newline to the
sink's stdin. The response must be one JSON object with the same `job_id`,
`ok: true`, and an object-valued `output`. Retryable errors, malformed output,
unexpected EOF, and process failure are recorded and leave the job pending;
non-retryable rejection marks it failed. Successful output is stored exactly in
SQLite and survives stopping and restarting the HTTP server.

The shared HMAC secret is never stored in SQLite or returned in JSON. The
worker does not use a shell, make network calls, or require third-party
packages.

## Checks

```text
python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v
python -m leasecascade --help
python -m leasecascade serve --help
python -m leasecascade worker --help
```

The independent fresh-process oracle is outside the package at
`../acceptance/test_lease_cascade.py` and must be run separately.
