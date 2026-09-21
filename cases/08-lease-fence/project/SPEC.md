# Lease Fence project specification

## Goal

Build `leasefence`, a small dependency-free Python 3 HTTP service that accepts
authenticated pipelines and delivers runnable jobs to a local sink. It uses
SQLite for durable state and a bounded `--once` worker. It is a lease-fencing
and fan-out/fan-in exercise, not a production scheduler or workflow engine.

The implementation must work from a clean checkout with no package
installation and may import only Python standard-library modules.

## Commands

Run commands from the project root:

```text
python -m leasefence serve --db PATH --secret SECRET --host HOST --port PORT
python -m leasefence worker --db PATH --sink PROGRAM [--sink-arg ARG]... --lease-seconds SECONDS --once
```

`serve` creates the SQLite database and parent directory when needed and stays
alive until stopped. The HMAC secret is used for authentication but must never
be stored in SQLite or returned in JSON.

`worker --once` claims runnable jobs in pipeline/job insertion order and exits
when no job is immediately available. It does not poll or wait for new work.
`--lease-seconds` is an integer from 1 through 300 and defaults to 30. A fresh
worker may reclaim an expired lease. A single invocation attempts a given job
at most once; a retryable attempt is deferred to a later invocation.

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

- `pipeline_id` and every `job_id` are non-empty ASCII ids containing only
  letters, digits, `.`, `_`, and `-`;
- `jobs` is a non-empty list and job ids are unique;
- each job has exactly `job_id`, `kind`, `payload`, `depends_on`, `input_refs`,
  and `collect`; `kind` is a non-empty trimmed string and `payload` is an
  object;
- `depends_on` is a list of unique job ids in the same pipeline; self-
  dependency is invalid and dependencies must be acyclic;
- `input_refs` is an object whose keys are valid ids and whose values are
  exactly `{"job_id": JOB_ID, "field": FIELD}`; each referenced job is named
  in `depends_on`; `field` is a non-empty top-level output key, never dotted;
- `collect` is either null or exactly `{"field": FIELD, "as": NAME}`;
- only `kind == "barrier"` may set `collect`; a barrier must set it and have
  at least two direct dependencies; non-barriers must set it to null;
- malformed JSON, unknown/missing fields, wrong types, duplicate ids, missing
  dependencies, invalid references, invalid `collect`, or cycles return `400`
  without inserting any pipeline or job;
- the first accepted pipeline returns `202`, with all jobs `pending`, zero
  attempts, and null output;
- repeating the exact same `pipeline_id` and canonical job content is
  idempotent: return `200` with the existing pipeline and do not duplicate it;
- reusing a `pipeline_id` with different job content returns `409` and leaves
  the existing pipeline unchanged;
- pipeline and jobs are inserted atomically; response key/list ordering is
  deterministic; payloads, references, collection declarations, and outputs
  survive restart.

### `GET /pipelines/<pipeline_id>`

Return `200` with original job order and this job shape:

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

Job status is one of `pending`, `leased`, `succeeded`, `failed`, or
`blocked`. An expired lease is made available before a status is returned, so
a fresh GET eventually shows `pending`. `attempts` counts claims, including a
claim whose worker dies before receiving a sink response. `last_error` is null
until a failed attempt, lease reclaim, or local input/collection failure; a
successful job clears it. `lease_expires_at` is null unless actively leased
and otherwise is an integer Unix epoch timestamp in milliseconds.

Aggregate pipeline status is `succeeded` when all jobs succeeded, `failed`
when any job failed or blocked, and `pending` otherwise, including while a job
is leased. Unknown pipeline ids return `404` with an `error` field.

The private claim fencing token is never returned by this endpoint and never
included in sink requests. It is durable only inside the worker state needed
to reject stale finalization.

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

`inputs` is formed only from declared `input_refs` and successful dependency
outputs. No undeclared output is copied. For a barrier, `fan_in` is instead:

```json
{
  "as": "artifacts",
  "items": [
    {"job_id": "part-a", "value": "artifact-release-1-part-a"},
    {"job_id": "part-b", "value": "artifact-release-1-part-b"}
  ]
}
```

Items use declared dependency order and contain only the selected top-level
field. A barrier is not runnable until all direct dependencies succeed. A
missing selected field marks the barrier `failed` locally, does not invoke its
sink, and blocks downstream jobs.

The sink reads exactly one JSON response line. A success is an object with a
matching `job_id`, `ok: true`, and object-valued `output`:

```json
{"job_id":"part-a","ok":true,"output":{"artifact_id":"a-1","private":"kept"}}
```

`ok: false` is a failed attempt; absent `retryable` means true. A retryable
rejection, malformed response, unexpected EOF, invalid successful output, or
sink process failure increments attempts, records a non-empty error, clears
the lease, leaves the job pending, continues bounded cleanup, and makes the
worker exit non-zero. A non-retryable rejection marks the job failed;
dependents become blocked when evaluated. A successful delivery stores the
exact output, clears lease/error, and may make downstream jobs runnable in the
same `--once` invocation. Succeeded jobs are never sent again.

## Lease fencing contract

When a worker claims a pending job, the store must atomically:

1. set status to `leased`;
2. increment `attempts`;
3. set `lease_expires_at`; and
4. store a fresh private, unguessable claim token.

Every finalization (`success`, retryable failure, or terminal failure) must use
that exact token in a conditional update that also requires `status ==
leased`. A finalization is accepted only when one row is updated.

When a lease expires, reclaim sets the job back to `pending`, clears the old
token, and records a reclaim diagnostic. A later worker receives a different
token. If the old worker eventually returns, its conditional update affects no
row: it must not change status, output, attempts, lease, or `last_error`. The
old worker reports a concise stale-lease outcome and exits non-zero. This is a
fencing rule, not exactly-once delivery; the old sink may already have observed
the request, so the durable state must not pretend that side effect was
undone.

The stale completion rule is the new Lease Fence behavior. It must hold across
separate fresh worker processes using the same SQLite database. No broad
guarantee about arbitrary concurrent workers is required beyond this bounded
claim-expiry race.

The worker must never poll forever, use a shell, make a network call, or
require third-party packages.

## Evaluation

The project is complete only when:

- from `project/`,
  `python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v`
  passes;
- the independent `../acceptance/test_lease_fence.py` suite passes;
- fresh processes reject invalid signatures, atomically admit a valid pipeline,
  deliver ordered fan-out/barrier/downstream jobs, preserve declared data
  flow, persist retry/blocked states, reclaim a lease, and fence a stale
  completion;
- `README.md` documents commands, signing, routes, states, leases/reclaim,
  private fencing behavior, sink protocol, persistence, and exact checks;
- only Python standard-library imports are used.

## Deliberate non-goals

- multiple users, authorization beyond one shared HMAC secret, HTTPS, replay
  windows, key rotation, or deployment packaging;
- migrations, multiple databases, scheduled retries, polling daemons,
  cancellation APIs, or a general workflow/scheduler language;
- arbitrary graph editing, nested references, more than one selected field per
  barrier, exactly-once delivery, or a general concurrent-worker guarantee;
- arbitrary sink protocols, shell pipelines, external network services, or
  changes to the `rupi` runtime/source/tests.
