# Receipt Ledger project specification

## Goal

Build `receiptledger`, a small dependency-free Python 3 HTTP service that
accepts authenticated pipelines and delivers runnable jobs to a local sink.
It uses SQLite for durable state and a bounded `--once` worker. This is a
lease, fencing, receipt, and tamper-evident operational-audit exercise, not a
production scheduler or workflow engine.

The implementation must run from a clean checkout with no package installation
and may import only Python standard-library modules.

## Commands

Run commands from the project root:

```text
python -m receiptledger serve --db PATH --secret SECRET --host HOST --port PORT
python -m receiptledger worker --db PATH --sink PROGRAM [--sink-arg ARG]... --lease-seconds SECONDS --once
python -m receiptledger audit --db PATH --verify
python -m receiptledger audit --db PATH --tail COUNT
```

`serve` creates the SQLite database and parent directory when needed and stays
alive until stopped. The HMAC secret authenticates requests but is never stored
in SQLite, returned in JSON, passed to the sink, or written to the audit log.

`worker --once` claims runnable jobs in pipeline/job insertion order and exits
when no job is immediately available. It does not poll or wait for new work.
A fresh worker may reclaim an expired lease. A single invocation attempts a
given job at most once; a retryable attempt is deferred to a later invocation.
`--lease-seconds` is an integer from 1 through 300 and defaults to 30.

The worker must invoke `PROGRAM` with the exact argument vector
`[PROGRAM, ARG1, ...]`, never through a shell. Values beginning with `-` are
data when passed through `--sink-arg`; `--sink-arg=VALUE` is allowed when an
argument resembles a worker option.

The top-level command and `serve`, `worker`, and `audit` subcommands must
provide useful `--help` output. `audit --verify` is read-only and exits zero
only when the full audit ledger is intact. `audit --tail COUNT` is also
read-only, prints at most `COUNT` safe canonical event objects, and does not
verify or mutate state unless `--verify` is supplied.

## Authentication and HTTP contract

Successful responses are JSON with `Content-Type: application/json`. Error
responses are JSON objects of the form `{"error": "..."}` with an appropriate
4xx status.

For `POST /pipelines`, the client sends the exact UTF-8 body and an
`X-Pipeline-Signature` header. The valid header is:

```text
sha256=<lowercase hexadecimal HMAC-SHA256 digest>
```

The digest is over exact raw body bytes using the server `SECRET` as the UTF-8
HMAC key. Signature comparison is constant-time. Missing, malformed, or
incorrect signatures return `401` and must not change the database or audit
ledger.

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
  and `collect` fields; `kind` is a non-empty trimmed string and `payload` is
  an object;
- `depends_on` is a list of unique job ids in the same pipeline; self-
  dependency is invalid and dependencies must be acyclic;
- `input_refs` is an object whose keys are valid ids and whose values are
  exactly `{"job_id": JOB_ID, "field": FIELD}`; each referenced job is named
  in `depends_on`; `field` is a non-empty top-level output key, never dotted;
- `collect` is either null or exactly `{"field": FIELD, "as": NAME}`;
- only `kind == "barrier"` may set `collect`; a barrier must set it and have
  at least two direct dependencies; non-barriers must set it to null;
- malformed JSON, unknown/missing fields, wrong types, empty values, duplicate
  ids, missing dependencies, invalid references, invalid `collect`, or cycles
  return `400` without inserting any pipeline, job, or audit row;
- the first accepted pipeline returns `202`, with all jobs `pending`, zero
  attempts, null output and receipt;
- repeating the exact same `pipeline_id` and canonical job content is
  idempotent: return `200` with the existing pipeline and do not duplicate it
  or append a second admission event;
- reusing a `pipeline_id` with different job content returns `409` and leaves
  the existing pipeline, jobs, and audit ledger unchanged;
- the pipeline, jobs, and first `pipeline_admitted` audit event are committed
  atomically; response key/list ordering is deterministic; payloads,
  references, collection declarations, outputs, receipts, and audit rows
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
      "delivery_key": "release-1:source",
      "status": "pending",
      "attempts": 0,
      "last_error": null,
      "lease_expires_at": null,
      "output": null,
      "receipt": null
    }
  ]
}
```

`delivery_key` is deterministic from validated pipeline and job ids using the
literal `pipeline_id + ":" + job_id`. It is stable across retries, reclaims,
and restarts. It is not a lease token, contains no secret, and is the only
delivery identity a sink needs for idempotent application.

Job status is one of `pending`, `leased`, `succeeded`, `failed`, or `blocked`.
An expired lease is made available before a status is returned, so a fresh GET
eventually shows `pending`. `attempts` counts claims, including a claim whose
worker dies or loses the sink response. `last_error` is null until a failed
attempt, lease reclaim, or local input/collection failure; a successful job
clears it. `lease_expires_at` is null unless actively leased and otherwise is
an integer Unix epoch timestamp in milliseconds.

`receipt` is null until accepted success, then is an object containing the
matching `delivery_key` and non-empty sink `receipt_id`. The private claim
token is never returned by this endpoint.

Aggregate pipeline status is `succeeded` when all jobs succeeded, `failed` when
any job failed or blocked, and `pending` otherwise, including while a job is
leased. Unknown pipeline ids return `404` with an `error` field.

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
  "fan_in": null,
  "delivery_key": "release-1:part-a"
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

The sink reads exactly one JSON response line. A successful response is an
object with matching `job_id`, matching `delivery_key`, `ok: true`, object-
valued `output`, and a non-empty string `receipt_id`:

```json
{
  "job_id": "part-a",
  "delivery_key": "release-1:part-a",
  "ok": true,
  "output": {"artifact_id": "a-1", "private": "kept"},
  "receipt_id": "receipt-release-1-part-a"
}
```

The sink may persist the logical side effect and then lose the response. A
later attempt receives the same `delivery_key`; an idempotent sink returns the
previous `output` and `receipt_id` without applying that key twice. The worker
does not turn EOF into success or invent a receipt. It records a retryable
failure with observed `outcome: "unknown"` when it can persist that result,
clears the lease, exits non-zero, and lets a later fresh worker recover through
the stable key.

`ok: false` is a failed attempt; absent `retryable` means true. A retryable
rejection, malformed response, unexpected EOF, invalid successful output, or
sink process failure increments attempts, records a non-empty error, clears
the lease, leaves the job `pending`, continues bounded cleanup, and makes the
worker exit non-zero. A non-retryable rejection marks the job `failed`; its
dependents become `blocked` when evaluated. A successful delivery stores the
exact output and receipt, clears lease/error, and may make downstream jobs
runnable in the same `--once` invocation. Succeeded jobs are never sent again.

## Lease fencing and receipt recovery

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
row: it must not change status, output, attempts, lease, last error, or
receipt. The old worker reports a concise stale-lease outcome and exits
non-zero.

The stable delivery key is deliberately separate from the private claim token.
It survives reclaim so the sink recognizes the same logical delivery, while the
private token prevents an old worker from finalizing newer durable state. This
is a bounded idempotent-sink contract, not exactly-once delivery: an arbitrary
sink that ignores delivery keys may still observe duplicate requests.

## Tamper-evident audit ledger

The SQLite database contains an `audit_events` table. Audit rows are not a
second source of job state; they are an append-only evidence projection written
in the same transaction as each authoritative mutation.

Each row has:

- `seq`, an integer starting at 1 with no gaps;
- `event_json`, a canonical UTF-8 JSON object with sorted keys and compact
  separators;
- `prev_hash`, equal to the prior row's `event_hash` or 64 zeroes for row 1;
- `event_hash`, equal to
  `sha256((prev_hash + "\\n" + event_json).encode("utf-8")).hexdigest()`.

The event object includes `seq`, bounded `kind`, safe pipeline/job/delivery
identifiers where relevant, observed public status/attempts, and a concise
safe detail/outcome. It excludes the HMAC secret, private claim token, raw
command argv, and arbitrary sink response bytes. The bounded kinds are
`pipeline_admitted`, `job_claimed`, `lease_reclaimed`, `job_succeeded`,
`job_retryable_failure`, `job_failed`, `job_blocked`, and
`stale_finalization_rejected`.

Admission and every job transition that changes durable state must append the
corresponding event in the same SQLite transaction. A stale finalization is a
rejected operational event and must not alter the newer job row. Duplicate
admission responses do not append events. The audit ledger must retain the
same stable delivery key on retries/reclaims and never expose claim tokens.

`audit --verify` opens the database read-only, checks table rows in sequence,
recomputes every hash, rejects missing/reordered/edited rows, and exits nonzero
with an actionable error on any mismatch. It never repairs or truncates the
ledger. `audit --tail COUNT` is a read-only projection of safe event objects.
Audit output is never fed to the worker and cannot cause a sink invocation.

## Evaluation

The project is complete only when:

- from `project/`,
  `python -W error::ResourceWarning -m unittest discover -s tests -p
  "test_*.py" -v` passes;
- the independent `../acceptance/test_receipt_ledger.py` suite passes;
- fresh processes reject invalid signatures, atomically admit a valid pipeline,
  deliver ordered fan-out/barrier/downstream jobs, preserve declared data
  flow, persist retry/blocked states, reclaim a crashed lease, fence stale
  completion, and recover one lost acknowledgement through a stable receipt;
- `audit --verify` passes before and after restart, its chain is independently
  recomputed by the oracle, and a tampered row is rejected;
- `README.md` documents commands, signing, routes, states, leases/reclaim,
  private fencing, delivery keys, receipt recovery, audit hash rules, sink
  protocol, persistence, and exact checks;
- only Python standard-library imports are used.

## Deliberate non-goals

- multiple users, authorization beyond one shared HMAC secret, HTTPS, replay
  windows, key rotation, or deployment packaging;
- concurrent-worker guarantees, migrations, multiple databases, scheduled
  retries, polling daemons, cancellation APIs, or arbitrary workflow languages;
- arbitrary graph editing, nested reference expressions, more than one
  selected field per barrier, exactly-once delivery, or a general audit query
  language;
- arbitrary sink protocols, shell pipelines, external network services, or
  changes to rupi runtime/source/tests.
