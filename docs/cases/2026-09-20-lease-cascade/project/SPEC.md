# Lease Cascade project specification

## Goal

Build `leasecascade`, a small dependency-free Python 3 HTTP service that
accepts authenticated pipelines and delivers runnable jobs to a local sink.
The service uses SQLite for durable state and a bounded `--once` worker. It is a
fan-out/fan-in data-flow exercise, not a production scheduler or workflow
engine.

The implementation must work from a clean checkout with no package
installation and may import only Python standard-library modules.

## Commands

Run commands from the project root:

```text
python -m leasecascade serve --db PATH --secret SECRET --host HOST --port PORT
python -m leasecascade worker --db PATH --sink PROGRAM [--sink-arg ARG]... --lease-seconds SECONDS --once
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
      "job_id": "source",
      "kind": "source",
      "payload": {"batch": "b-1"},
      "depends_on": [],
      "input_refs": {},
      "collect": null
    },
    {
      "job_id": "part-a",
      "kind": "part",
      "payload": {"name": "a"},
      "depends_on": ["source"],
      "input_refs": {
        "source_id": {"job_id": "source", "field": "artifact_id"}
      },
      "collect": null
    },
    {
      "job_id": "part-b",
      "kind": "part",
      "payload": {"name": "b"},
      "depends_on": ["source"],
      "input_refs": {
        "source_id": {"job_id": "source", "field": "artifact_id"}
      },
      "collect": null
    },
    {
      "job_id": "barrier",
      "kind": "barrier",
      "payload": {"name": "join"},
      "depends_on": ["part-a", "part-b"],
      "input_refs": {},
      "collect": {"field": "artifact_id", "as": "artifacts"}
    },
    {
      "job_id": "publish",
      "kind": "publish",
      "payload": {"channel": "stable"},
      "depends_on": ["barrier"],
      "input_refs": {
        "joined": {"job_id": "barrier", "field": "joined"}
      },
      "collect": null
    }
  ]
}
```

Rules:

- `pipeline_id` and every `job_id` are valid non-empty ASCII ids containing
  only letters, digits, `.`, `_`, and `-`;
- `jobs` is a non-empty list and job ids are unique;
- each job has exactly `job_id`, `kind`, `payload`, `depends_on`, `input_refs`,
  and `collect` fields; `kind` is a non-empty trimmed string and `payload` is
  a JSON object;
- `depends_on` is a list of unique job ids in the same pipeline; self-
  dependency is invalid and dependencies must be acyclic;
- `input_refs` is an object whose keys are non-empty valid ids and whose values
  are exactly `{"job_id": JOB_ID, "field": FIELD}`; every referenced job is
  also named in `depends_on`; `field` is a non-empty string naming a top-level
  output key, never a dotted path;
- `collect` is either `null` or exactly `{"field": FIELD, "as": NAME}`;
  `field` is a non-empty top-level output key and `as` is a valid non-empty id;
- only a job with `kind` equal to `barrier` may set `collect`; a `barrier` must
  set a non-null `collect` and have at least two direct dependencies;
- a non-barrier job must have `collect: null`; a barrier's direct dependencies
  are the fan-out results it will collect, in the declared `depends_on` order;
- malformed JSON, unknown/missing fields, wrong types, empty values, duplicate
  ids, missing dependencies, invalid references, invalid `collect`, or cycles
  return `400` without inserting any pipeline or job;
- the first accepted pipeline returns `202` and all jobs are `pending` with
  zero attempts and null `output`;
- repeating the exact same `pipeline_id` and canonical job content is
  idempotent: return `200` with the existing pipeline and do not create a
  second pipeline;
- reusing a `pipeline_id` with different job content returns `409` and leaves
  the existing pipeline unchanged;
- the pipeline and all jobs are inserted atomically; responses have
  deterministic key and list ordering; payloads, references, collection
  declarations, and outputs survive restart.

### `GET /pipelines/<pipeline_id>`

Return `200` with this shape, keeping original job order:

```json
{
  "pipeline_id": "release-1",
  "status": "pending",
  "jobs": [
    {
      "job_id": "source",
      "kind": "source",
      "payload": {"batch": "b-1"},
      "depends_on": [],
      "input_refs": {},
      "collect": null,
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
failed sink attempt, lease reclaim, or local input/collection-resolution
failure; a successful job clears it. `lease_expires_at` is null unless actively
leased and otherwise is an integer Unix epoch timestamp in milliseconds.

Aggregate pipeline status is:

- `succeeded` when every job is `succeeded`;
- `failed` when any job is `failed` or `blocked`;
- `pending` otherwise, including while a job is `leased`.

Unknown pipeline ids return `404` with an `error` field.

## Sink protocol and worker behavior

For every claimed job, the worker starts one sink process and writes exactly
one UTF-8 JSON object followed by `\n` to stdin:

```json
{
  "pipeline_id": "release-1",
  "job_id": "part-a",
  "kind": "part",
  "payload": {"name": "a"},
  "depends_on": ["source"],
  "inputs": {"source_id": "artifact-release-1-source"},
  "fan_in": null
}
```

`inputs` is formed only from each declared `input_refs` entry and successful
dependency output. No undeclared output is copied into the request.

For a barrier, `fan_in` is instead:

```json
{
  "as": "artifacts",
  "items": [
    {"job_id": "part-a", "value": "artifact-release-1-part-a"},
    {"job_id": "part-b", "value": "artifact-release-1-part-b"}
  ]
}
```

The items use the barrier's declared dependency order. Only the selected
top-level `collect.field` is included; other dependency output keys are not
copied into `items`. A barrier is not runnable until every direct dependency is
successful. If the selected field is absent from a successful dependency, the
worker must not invoke the barrier sink: it marks the barrier `failed` with a
concise local error and blocks its dependents.

The sink reads exactly one JSON object line in response. A successful response
is an object with matching `job_id`, boolean `ok: true`, and an object-valued
`output`:

```json
{"job_id":"part-a","ok":true,"output":{"artifact_id":"a-1","private":"kept"}}
```

A response with `ok: false` is a failed attempt; absent `retryable` means
`true`. A retryable rejection, malformed response, unexpected EOF, invalid
successful output, or sink process failure increments attempts, records a
non-empty error, clears the lease, leaves the job `pending`, continues bounded
cleanup, and makes the worker exit non-zero. A non-retryable rejection marks
the job `failed`; dependents become `blocked` when dependencies are evaluated.

Successful delivery increments and retains attempts, stores the exact output,
clears lease/error, and may make downstream jobs runnable in the same `--once`
invocation. A mismatched `job_id` is a failed retryable attempt and must not
mark another job succeeded. A succeeded job is never sent again.

A worker killed after claim commit but before finalization leaves a `leased`
job until expiry; a later worker reclaims and delivers it. Jobs with pending or
leased dependencies are not claimed. Jobs with failed or blocked dependencies
become `blocked` and are never sent. The worker exits zero when there are no
runnable jobs or every attempted job succeeds, and non-zero if any attempt
fails or the invocation is invalid. It must not poll forever, use a shell,
make a network call, or require third-party packages.

## Evaluation

The project succeeds only when all of the following are true:

- from `project/`,
  `python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v`
  passes;
- the independent oracle in `../acceptance/test_lease_cascade.py` passes;
- fresh processes reject invalid signatures, atomically admit a valid pipeline,
  deliver source/fan-out/barrier/downstream order, preserve selected fan-in
  ordering, propagate declared output fields, persist retry and blocked states,
  reclaim a crashed lease, and restart against the same SQLite file;
- invalid authentication, invalid JSON, invalid collection declarations, and
  missing selected output fields cannot silently mutate a successful cascade;
- `README.md` documents commands, signing, routes, job states, `collect` and
  `fan_in`, leases/reclaim, sink protocol, persistence, and exact checks;
- only Python standard-library imports are used.

## Deliberate non-goals

- multiple users, authorization beyond one shared HMAC secret, HTTPS, replay
  windows, key rotation, or deployment packaging;
- concurrent-worker guarantees, distributed locking, migrations, multiple
  database backends, scheduled retries, exponential backoff, polling daemons,
  or cancellation APIs;
- arbitrary graph editing, nested reference expressions, more than one
  selected field per barrier, or a general map/reduce language;
- arbitrary sink protocols, shell pipelines, external network services, or
  exactly-once delivery semantics.
