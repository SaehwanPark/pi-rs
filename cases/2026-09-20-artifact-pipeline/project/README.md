# Artifact Pipeline

Artifact Pipeline is a dependency-free Python service for accepting signed
pipelines and delivering runnable jobs to a local process. SQLite stores the
pipeline, job state, leases, attempts, and JSON outputs. The worker is a
bounded `--once` pass; it is not a daemon or a message broker.

## Start the service

From this directory:

```text
python -m artifactpipe serve --db state/pipeline.sqlite3 --secret development-secret --host 127.0.0.1 --port 8787
```

`POST /pipelines` requires `X-Pipeline-Signature:
sha256=<lowercase HMAC-SHA256 hex digest>`, where the digest covers the exact
UTF-8 request body and uses the server secret as the UTF-8 key. The secret is
never written to SQLite or returned by the API.

The service exposes `GET /healthz`, `POST /pipelines`, and
`GET /pipelines/<pipeline_id>`. A pipeline contains ordered jobs with a
`payload`, `depends_on`, and `input_refs`. A reference names one top-level
field in a successful dependency output and must also be declared as a
dependency. Cycles, missing dependencies, malformed JSON, and unknown fields
are rejected atomically. Repeating the same pipeline is idempotent; changing
its content returns `409`.

## Run the bounded worker

The worker invokes the sink with direct argv values and never starts a shell:

```text
python -m artifactpipe worker --db state/pipeline.sqlite3 --sink python --sink-arg sink.py --sink-arg=--log --sink-arg=delivery.log --lease-seconds 30 --once
```

For each claimed job it sends one JSON line containing `pipeline_id`, `job_id`,
`kind`, `payload`, `depends_on`, and resolved `inputs`. The sink returns one
JSON object line with the matching `job_id`, `ok: true`, and an object-valued
`output`. The output is retained in SQLite and can feed explicitly declared
fields to later jobs.

Jobs wait for all dependencies to succeed. A retryable rejection, malformed
response, unexpected EOF, invalid successful output, or sink failure leaves a
job pending for a later worker invocation and gives that invocation a non-zero
status. `ok: false, retryable: false` marks a job failed and blocks dependents.
An expired lease is reclaimed by a fresh worker. A killed worker therefore
does not silently turn an unobserved delivery into success.

## Tests and boundaries

Run the project suite from this directory:

```text
python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v
```

The independent fresh-process oracle is run from the case root:

```text
python -W error::ResourceWarning -m unittest discover -s acceptance -p "test_*.py" -v
```

Only Python standard-library modules are used. The full HTTP contract and
state-machine details are in [SPEC.md](SPEC.md).
