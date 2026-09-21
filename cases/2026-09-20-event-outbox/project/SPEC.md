# Event Outbox project specification

## Goal

Build `outbox`, a small dependency-free Python 3 HTTP service that durably
accepts events and delivers them through a separate worker process. The project
must be readable, runnable from a clean checkout, and testable with only the
Python standard library.

This is a bounded integration example, not a production message broker.

## Commands

Run commands from the project root:

```text
python -m outbox serve --db PATH --host HOST --port PORT
python -m outbox worker --db PATH --sink PROGRAM [--sink-arg ARG]... --once
```

`--sink-arg` may be repeated. The worker must pass `PROGRAM` and its argument
values directly to the child process without a shell. The worker starts one sink
process for one `--once` invocation, closes it before exiting, and processes
all currently pending events in insertion order.

The service creates the SQLite database and its parent directory when needed.
The server stays alive until it is stopped. The worker is bounded: `--once`
always exits after the pending snapshot is attempted and must not spin forever.

The project must provide useful `--help` output for the top-level command and
both subcommands. It must import only Python standard-library modules.

## HTTP contract

Successful HTTP responses are JSON with `Content-Type: application/json`,
except `204` responses, which have no body. Error responses are JSON objects of
the form `{"error": "..."}` with an appropriate 4xx status.

### `GET /healthz`

Return `200` and `{"ok": true}` without requiring any events.

### `GET /events/<event_id>`

Return `200` and an object with these fields:

```json
{
  "event_id": "evt-1",
  "topic": "release",
  "payload": {"version": "0.3.0"},
  "status": "pending",
  "attempts": 0,
  "last_error": null
}
```

`status` is `pending` or `delivered`. `attempts` counts delivery attempts,
including attempts that received a negative sink response. `last_error` is null
until a failed attempt and is a concise non-empty string after one.

An unknown event id returns `404` with an `error` field.

### `POST /events`

Accept exactly this JSON object shape:

```json
{
  "event_id": "evt-1",
  "topic": "release",
  "payload": {"version": "0.3.0"}
}
```

Rules:

- `event_id` and `topic` are non-empty strings after trimming;
- `event_id` contains only ASCII letters, digits, `.`, `_`, and `-`;
- `payload` is a JSON object;
- unknown fields, missing fields, malformed JSON, wrong types, and empty values
  return `400` and do not create an event;
- the first accepted event returns `202` and its event object with `pending`
  status and zero attempts;
- repeating the exact same `event_id`, `topic`, and `payload` is idempotent:
  return `200` with the existing event and do not create a second delivery;
- reusing an existing `event_id` with a different topic or payload returns
  `409` with an `error` field and leaves the existing event unchanged.

The response must use deterministic JSON key and list ordering where ordering is
observable. Event payload data must survive a server restart unchanged.

Other paths and methods return JSON `404`/`405` errors as appropriate.

## Sink protocol and worker behavior

For each pending event, the worker writes exactly one UTF-8 JSON object followed
by `\n` to the sink process's stdin:

```json
{"event_id":"evt-1","topic":"release","payload":{"version":"0.3.0"}}
```

The worker reads exactly one JSON object line in response. A delivery succeeds
only when the response is an object with the matching `event_id` and boolean
`ok: true`, for example:

```json
{"event_id":"evt-1","ok":true}
```

A response with `ok: false`, a mismatched id, malformed JSON, an unexpected EOF,
or a sink failure is a failed attempt. The worker must record a non-empty error,
increment `attempts`, leave the event `pending`, continue its bounded cleanup,
and exit non-zero if any attempted event failed. A successful response records
the incremented attempt count and changes the event to `delivered`.

The worker exits zero when every attempted event succeeds, including when there
were no pending events. It exits non-zero for a delivery failure or invalid
worker invocation. A delivered event is never sent again by later `--once` runs.

The worker must use direct process arguments; shell metacharacters in a sink
argument are data, not executable syntax. The protocol is intentionally local
and line-delimited; no network call or third-party package is required.

## Evaluation

The project succeeds when all of the following are true:

- from `project/`,
  `python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v`
  passes;
- the independent oracle in `../acceptance/test_event_outbox.py` passes;
- a fresh process can admit events, run a separate sink-backed worker, observe
  delivery and retry state, and restart the server against the same database;
- invalid HTTP input cannot mutate an accepted event;
- the README documents both commands, the HTTP endpoints, the sink protocol,
  persistence, and exact test commands;
- only Python standard-library imports are used.

## Deliberate non-goals

- authentication, authorization, HTTPS, rate limiting, or deployment packaging;
- multiple database backends, migrations, or schema versioning;
- concurrent workers, locking guarantees, or distributed delivery;
- a daemon mode, scheduled retries, exponential backoff, or a job dashboard;
- arbitrary sink protocols, shell pipelines, or external network services;
- exactly-once delivery semantics (the durable contract is at-least-once for
  pending events, with explicit attempts and retry state).
