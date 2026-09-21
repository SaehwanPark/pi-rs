# Artifact Pipeline project specification

## Goal

Build `artifactpipe`, a small dependency-free Python 3 HTTP service that
accepts authenticated pipelines and delivers runnable jobs to a local sink.
The service uses SQLite for durable state and a bounded `--once` worker. It is
a data-flow exercise, not a production workflow engine.

The implementation must work from a clean checkout with no package
installation and may import only Python standard-library modules.

## Commands

Run commands from the project root:

```text
python -m artifactpipe serve --db PATH --secret SECRET --host HOST --port PORT
python -m artifactpipe worker --db PATH --sink PROGRAM [--sink-arg ARG]... --lease-seconds SECONDS --once
```

`serve` creates the SQLite database and parent directory when needed and stays
alive until stopped. The secret is used for HMAC verification but must never be
stored in SQLite or returned in JSON.

`worker --once` claims runnable jobs in pipeline/job insertion order and exits
when none are immediately available. It does not poll or wait for new work.
`--lease-seconds` is an integer from 1 through 300 and defaults to 30. A fresh
worker may reclaim a job whose lease expired. A single invocation attempts a
given job at most once; a retryable attempt is deferred to a later invocation.

The worker must invoke `PROGRAM` with the exact argument vector
`[PROGRAM, ARG1, ...]`, never through a shell. Values beginning with `-` are
data when passed through `--sink-arg`; `--sink-arg=VALUE` is allowed when an
argument resembles a worker option.

The top-level command and both subcommands must provide useful `--help`
output.

## Authentication and HTTP contract

Successful responses are JSON with `Content-Type: application/json`. Error
responses are JSON objects of the form `{"error": "..."}` with an appropriate
4xx status.

For `POST /pipelines`, the client sends the exact UTF-8 body and an
`X-Pipeline-Signature` header. The valid header is:

```text
sha256=<lowercase hexadecimal HMAC-SHA256 digest>
```

The digest is over the exact raw body bytes using the server `SECRET` as the
UTF-8 HMAC key. Signature comparison is constant-time. Missing, malformed, or
incorrect signatures return `401` and must not change the database.

### `GET /healthz`

Return `200` and `{"ok": true}`.

### `POST /pipelines`

After signature verification, accept exactly this JSON object shape:

```json
{
  "pipeline_id": "release-1",
  "jobs": [
    {
      "job_id": "build",
      "kind": "build",
      "payload": {"version": "1.2.3"},
      "depends_on": [],
      "input_refs": {}
    },
    {
      "job_id": "publish",
      "kind": "publish",
      "payload": {"channel": "stable"},
      "depends_on": ["build"],
      "input_refs": {
        "artifact_id": {"job_id": "build", "field": "artifact_id"}
      }
    }
  ]
}
```

Rules:

- `pipeline_id` is a valid non-empty ASCII id;
- `jobs` is a non-empty list;
- each job has exactly `job_id`, `kind`, `payload`, `depends_on`, and
  `input_refs` fields;
- job ids are unique valid ids; `kind` is a non-empty trimmed string;
- `payload` is a JSON object;
- `depends_on` is a list of unique job ids in the same pipeline;
- `input_refs` is an object whose keys are non-empty valid ids and whose values
  are exactly `{"job_id": JOB_ID, "field": FIELD}`;
- every referenced job is also named in that job's `depends_on` list;
- `field` is a non-empty string naming a top-level key, not a dotted path;
- dependencies form an acyclic graph, with self-dependency invalid;
- malformed JSON, unknown/missing fields, wrong types, empty values, duplicate
  ids, missing dependencies, invalid references, or cycles return `400` without
  inserting any pipeline or job;
- the first accepted pipeline returns `202` and all jobs `pending` with zero
  attempts and null `output`;
- repeating the exact same `pipeline_id` and canonical job content is
  idempotent: return `200` with the existing pipeline and do not create a
  second pipeline;
- reusing a `pipeline_id` with different job content returns `409` and leaves
  the existing pipeline unchanged;
- the pipeline and all jobs are inserted atomically; responses have
  deterministic key and list ordering; payloads, references, and outputs
  survive restart.

### `GET /pipelines/<pipeline_id>`

Return `200` with this shape, keeping original job order:

```json
{
  "pipeline_id": "release-1",
  "status": "pending",
  "jobs": [
    {
      "job_id": "build",
      "kind": "build",
      "payload": {"version": "1.2.3"},
      "depends_on": [],
      "input_refs": {},
      "status": "pending",
      "attempts": 0,
      "last_error": null,
      "lease_expires_at": null,
      "output": null
    }
  ]
}
```

Job status is one of `pending`, `leased`, `succeeded`, `failed`, or `blocked`.
An expired lease is made available before a status is returned, so a fresh GET
eventually shows `pending`. `attempts` counts claims, including a claim whose
worker dies before receiving a sink response. `last_error` is null until a
failed sink attempt, lease reclaim, or local input-resolution failure; a
successful job clears it. `lease_expires_at` is null unless actively leased and
otherwise is an integer Unix epoch timestamp in milliseconds. `output` is null
until success and is then the sink's JSON object.

Aggregate pipeline status is:

- `succeeded` when every job is `succeeded`;
- `failed` when any job is `failed` or `blocked`;
- `pending` otherwise, including while a job is `leased`.

Unknown pipeline ids return `404` with an `error` field. Pipeline and job ids
use ASCII letters, digits, `.`, `_`, and `-`; ids are non-empty after trimming.

## Sink protocol and worker behavior

For every claimed job, the worker starts one sink process and writes exactly one
UTF-8 JSON object followed by `\n` to stdin:

```json
{
  "pipeline_id": "release-1",
  "job_id": "publish",
  "kind": "publish",
  "payload": {"channel": "stable"},
  "depends_on": ["build"],
  "inputs": {"artifact_id": "artifact-release-1-build"}
}
```

`inputs` is formed by resolving each declared `input_refs` entry from the
top-level fields of successful dependency `output` objects. No undeclared
output is copied into the request. The worker reads exactly one JSON object
line in response. A successful response is an object with matching `job_id`,
boolean `ok: true`, and an object-valued `output`:

```json
{"job_id":"publish","ok":true,"output":{"receipt":"r-1"}}
```

A response with `ok: false` is a failed attempt; absent `retryable` means
`true`. A retryable rejection, malformed response, unexpected EOF, invalid
successful output, or sink process failure increments attempts, records a
non-empty error, clears the lease, leaves the job `pending`, continues bounded
cleanup, and makes the worker exit non-zero. A non-retryable rejection marks
the job `failed`; dependents become `blocked` when dependencies are evaluated.

If a job's declared input field is absent from a successful dependency output,
the worker must not invoke the sink for that job. It records a concise local
error, marks the job `failed`, and blocks its dependents.

Successful delivery increments and retains attempts, stores the exact output,
clears lease/error, and may make downstream jobs runnable in the same
`--once` invocation. A mismatched `job_id` is a failed retryable attempt and
must not mark another job succeeded. A succeeded job is never sent again.

A worker killed after claim commit but before finalization leaves a `leased`
job until expiry; a later worker reclaims and delivers it. Jobs with pending or
leased dependencies are not claimed. Jobs with failed or blocked dependencies
become `blocked` and are never sent. The worker exits zero when there are no
runnable jobs or every attempted job succeeds, and non-zero if any attempt
fails or the invocation is invalid. It must not poll forever, use a shell, make
a network call, or require third-party packages.

## Evaluation

The project succeeds only when all of the following are true:

- from `project/`,
  `python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v`
  passes;
- the independent oracle in `../acceptance/test_artifact_pipeline.py` passes;
- fresh processes reject invalid signatures, atomically admit a valid pipeline,
  deliver dependency order, propagate declared output fields, persist retry and
  blocked states, reclaim a crashed lease, and restart against the same SQLite
  file;
- invalid authentication and invalid JSON cannot mutate an accepted pipeline;
- `README.md` documents commands, signing, routes, state transitions,
  references/outputs, lease/reclaim behavior, sink protocol, persistence, and
  exact checks;
- only Python standard-library imports are used.

## Deliberate non-goals

- multiple users, authorization beyond one shared HMAC secret, HTTPS, replay
  windows, key rotation, or deployment packaging;
- concurrent-worker guarantees, distributed locking, migrations, or multiple
  database backends;
- scheduled retries, exponential backoff, polling daemons, cancellation API,
  arbitrary graph editing, nested reference expressions, or a UI;
- arbitrary sink protocols, shell pipelines, or external network services;
- exactly-once delivery semantics.

