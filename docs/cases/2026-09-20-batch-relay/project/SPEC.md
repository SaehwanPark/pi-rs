# Batch Relay project specification

## Goal

Build `batchrelay`, a small dependency-free Python 3 HTTP service that
accepts authenticated batches of jobs and delivers them through a separate
bounded worker process. It is a scheduler exercise, not a production workflow
engine.

The implementation must use only the Python standard library and must work
from a clean checkout with no package installation.

## Commands

Run commands from the project root:

```text
python -m batchrelay serve --db PATH --secret SECRET --host HOST --port PORT
python -m batchrelay worker --db PATH --sink PROGRAM [--sink-arg ARG]... --lease-seconds SECONDS --once
```

`serve` creates the SQLite database and its parent directory when needed. The
server stays alive until it is stopped. The secret is supplied to the server
but must not be written to the database or included in response JSON.

`worker --once` repeatedly claims currently runnable jobs in batch/job
insertion order and exits when none are immediately available. It must not
poll forever or wait for new jobs. `--lease-seconds` is an integer from 1
through 300 and defaults to 30. A worker may reclaim a job whose lease has
expired.

The worker must invoke `PROGRAM` with the exact argument vector
`[PROGRAM, ARG1, ...]`, without a shell. Values beginning with `-` are data
when passed through `--sink-arg`; users may use the `--sink-arg=VALUE`
spelling when an argument looks like a worker option.

The top-level command and both subcommands must provide useful `--help`
output.

## Authentication and HTTP contract

All successful responses are JSON with `Content-Type: application/json`.
Error responses are JSON objects of the form `{"error": "..."}` with an
appropriate 4xx status.

For `POST /batches`, the client sends the exact UTF-8 request body and an
`X-Batch-Signature` header. The valid header is:

```text
sha256=<lowercase hexadecimal HMAC-SHA256 digest>
```

The digest is computed over the exact raw body bytes with the server's
`SECRET` as the UTF-8 HMAC key. Signature comparison must be constant-time.
A missing, malformed, or incorrect signature returns `401` and must not
change the database. The server verifies the signature before accepting the
request; an invalid signature must never become a successful write.

### `GET /healthz`

Return `200` and `{"ok": true}` without requiring any batches.

### `GET /batches/<batch_id>`

Return `200` and this shape, with jobs in their original submission order:

```json
{
  "batch_id": "batch-1",
  "status": "pending",
  "jobs": [
    {
      "job_id": "compile",
      "kind": "compile",
      "payload": {"version": "0.5.0"},
      "depends_on": [],
      "status": "pending",
      "attempts": 0,
      "last_error": null,
      "lease_expires_at": null
    }
  ]
}
```

Job status is one of `pending`, `leased`, `succeeded`, `failed`, or
`blocked`. An expired lease is made available again before a status is
returned, so a fresh GET eventually shows `pending`. `attempts` counts claims,
including a claim whose worker dies before receiving a sink response.
`last_error` is null until a failed sink attempt or lease reclaim and is a
concise non-empty string afterward. A successful job clears its error.
`lease_expires_at` is null unless the job is actively leased and otherwise is
an integer Unix epoch timestamp in milliseconds.

The aggregate batch status is:

- `succeeded` when every job is `succeeded`;
- `failed` when any job is `failed` or `blocked`;
- `pending` otherwise, including while a job is `leased`.

An unknown batch id returns `404` with an `error` field. Batch and job ids use
ASCII letters, digits, `.`, `_`, and `-`; ids are non-empty after trimming.

### `POST /batches`

After signature verification, accept exactly this JSON object shape:

```json
{
  "batch_id": "batch-1",
  "jobs": [
    {
      "job_id": "compile",
      "kind": "compile",
      "payload": {"version": "0.5.0"},
      "depends_on": []
    },
    {
      "job_id": "publish",
      "kind": "publish",
      "payload": {"channel": "stable"},
      "depends_on": ["compile"]
    }
  ]
}
```

Rules:

- `batch_id` is a valid non-empty id;
- `jobs` is a non-empty list;
- each job has exactly `job_id`, `kind`, `payload`, and `depends_on` fields;
- job ids are unique within the batch and valid ids;
- `kind` is a non-empty string after trimming;
- `payload` is a JSON object;
- `depends_on` is a list of unique job ids in the same batch;
- dependencies form an acyclic graph (self-dependency is invalid);
- malformed JSON, unknown/missing fields, wrong types, empty values, missing
  dependencies, duplicate ids, or cycles return `400` without inserting any
  job;
- the first accepted batch returns `202` and its batch object with all jobs
  `pending` and zero attempts;
- repeating the exact same `batch_id` and canonical job content is idempotent:
  return `200` with the existing batch and do not create a second batch;
- reusing an existing `batch_id` with different jobs returns `409` and leaves
  the existing batch unchanged.

The whole batch and all of its jobs must be inserted atomically. A failed
request must not leave a partial batch. Responses use deterministic JSON key
and list ordering. Job payloads survive a server restart unchanged.

Other paths and methods return JSON `404`/`405` errors as appropriate.

## Sink protocol and worker behavior

For each claimed job, the worker starts one sink process and writes exactly
one UTF-8 JSON object followed by `\n` to its stdin:

```json
{
  "batch_id": "batch-1",
  "job_id": "compile",
  "kind": "compile",
  "payload": {"version": "0.5.0"},
  "depends_on": []
}
```

The worker reads exactly one JSON object line in response. A successful
response is an object with the matching `job_id` and boolean `ok: true`:

```json
{"job_id":"compile","ok":true}
```

A response with `ok: false` is a failed attempt. It may include boolean
`retryable` and a short string `error`; absent `retryable` means `true`.

- retryable failure, malformed response, unexpected EOF, or sink failure:
  increment attempts, record a non-empty error, clear the lease, leave the
  job `pending`, continue bounded cleanup, and cause a non-zero worker exit;
  a job is attempted at most once by one `--once` invocation, so a retryable
  job is deferred to a later invocation;
- non-retryable failure (`ok: false, retryable: false`): increment attempts,
  record the error, clear the lease, mark the job `failed`, and cause every
  dependent job to become `blocked` once the worker evaluates dependencies;
- successful response: incremented attempts are retained, the job becomes
  `succeeded`, the lease and error are cleared, and newly unblocked jobs may
  run in the same `--once` invocation;
- a mismatched `job_id` is a failed retryable attempt and must not mark a
  different job succeeded;
- a delivered/succeeded job is never sent again by later `--once` runs;
- a worker process killed after the claim transaction commits but before the
  job is finalized leaves the job `leased` until its timestamp expires; a
  later worker must reclaim and deliver it;
- jobs with pending or leased dependencies are not claimed; jobs with a
  failed or blocked dependency become `blocked` and are never sent;
- the worker exits zero when there are no runnable jobs or every attempted job
  succeeds, and exits non-zero if any sink attempt fails or the invocation is
  invalid;
- the worker must not poll forever, use a shell, make a network call, or need
  a third-party package.

## Evaluation

The project succeeds when all of the following are true:

- from `project/`,
  `python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v`
  passes;
- the independent oracle in `../acceptance/test_batchrelay.py` passes;
- a fresh process can reject invalid signatures, atomically admit a valid DAG,
  deliver jobs in dependency order, persist retry and blocked states, reclaim a
  crashed lease, and restart against the same SQLite file;
- invalid authentication and invalid JSON cannot mutate an accepted batch;
- the README documents both commands, signing, the HTTP endpoint, state
  transitions, dependency behavior, lease/reclaim semantics, sink protocol,
  persistence, and exact test commands;
- only Python standard-library imports are used.

## Deliberate non-goals

- multiple users, authorization beyond one shared HMAC secret, HTTPS, replay
  windows, key rotation, or deployment packaging;
- concurrent worker guarantees, distributed locking, migrations, or multiple
  database backends;
- scheduled retries, exponential backoff, polling daemons, cancellation API,
  arbitrary graph editing, or a UI;
- arbitrary sink protocols, shell pipelines, or external network services;
- exactly-once delivery semantics;
- accepting arbitrary JSON fields or silently normalizing invalid requests.
