# Webhook Inbox project specification

## Goal

Build `webhookinbox`, a small dependency-free Python 3 HTTP service that
durably accepts authenticated webhook deliveries and sends them through a
separate bounded worker process. It is an integration exercise, not a
production webhook gateway.

The implementation must use only the Python standard library and must work
from a clean checkout with no package installation.

## Commands

Run commands from the project root:

```text
python -m webhookinbox serve --db PATH --secret SECRET --host HOST --port PORT
python -m webhookinbox worker --db PATH --sink PROGRAM [--sink-arg ARG]... --lease-seconds SECONDS --once
```

`serve` creates the SQLite database and its parent directory when needed. The
server stays alive until it is stopped. The secret is supplied to the server
but must not be written to the database or included in response JSON.

`worker --once` repeatedly claims currently available deliveries in insertion
order and exits when none are immediately available. It must not poll forever
or wait for new deliveries. `--lease-seconds` is an integer from 1 through 300
and defaults to 30. A worker may reclaim a delivery whose lease has expired.

The worker must invoke `PROGRAM` with the exact argument vector
`[PROGRAM, ARG1, ...]`, without a shell. Values beginning with `-` are data
when passed through `--sink-arg`; users may use the `--sink-arg=VALUE` spelling
when an argument looks like a worker option.

The top-level command and both subcommands must provide useful `--help` output.

## Authentication and HTTP contract

All successful responses are JSON with `Content-Type: application/json`, except
`204` responses if an implementation adds a documented health variant. Error
responses are JSON objects of the form `{"error": "..."}` with an appropriate
4xx status.

For `POST /deliveries`, the client sends the exact UTF-8 request body and an
`X-Webhook-Signature` header. The valid header is:

```text
sha256=<lowercase hexadecimal HMAC-SHA256 digest>
```

The digest is computed over the exact raw body bytes with the server's `SECRET`
as the UTF-8 HMAC key. Signature comparison must be constant-time. A missing,
malformed, or incorrect signature returns `401` and must not change the
database. The server must verify the signature before accepting the request;
it may reject malformed JSON with either `400` or `401` when the signature is
also invalid, but an invalid signature must never become a successful write.

### `GET /healthz`

Return `200` and `{"ok": true}` without requiring any deliveries.

### `GET /deliveries/<delivery_id>`

Return `200` and an object with these fields:

```json
{
  "delivery_id": "del-1",
  "event_type": "release.published",
  "payload": {"version": "0.4.0"},
  "status": "pending",
  "attempts": 0,
  "last_error": null,
  "lease_expires_at": null
}
```

`status` is `pending`, `leased`, or `delivered`. A lease that has expired is
made available again before a status is returned, so a fresh GET eventually
shows `pending`. `attempts` counts claims/delivery attempts, including a claim
whose worker dies before receiving a sink response. `last_error` is null until
a failed sink attempt and is a concise non-empty string afterward; a successful
delivery clears it. `lease_expires_at` is null unless the row is actively
leased, and otherwise is an integer Unix epoch timestamp in milliseconds.

An unknown delivery id returns `404` with an `error` field. Delivery ids in
paths use the same ASCII identifier grammar as the POST body.

### `POST /deliveries`

After signature verification, accept exactly this JSON object shape:

```json
{
  "delivery_id": "del-1",
  "event_type": "release.published",
  "payload": {"version": "0.4.0"}
}
```

Rules:

- `delivery_id` and `event_type` are non-empty strings after trimming;
- `delivery_id` contains only ASCII letters, digits, `.`, `_`, and `-`;
- `payload` is a JSON object;
- unknown fields, missing fields, malformed JSON, wrong types, and empty values
  return `400` and do not create or modify a delivery;
- the first accepted delivery returns `202` and its delivery object with
  `pending` status, zero attempts, and a null lease;
- repeating an existing `delivery_id` with the same `event_type` and payload is
  idempotent: return `200` with the existing object and do not create a second
  delivery or claim;
- reusing an existing `delivery_id` with a different event type or payload
  returns `409` with an `error` field and leaves the existing row unchanged.

Responses must use deterministic JSON key and list ordering where ordering is
observable. Payload data must survive a server restart unchanged.

Other paths and methods return JSON `404`/`405` errors as appropriate.

## Delivery lease and sink protocol

For each available delivery, the worker atomically claims the oldest row whose
status is `pending` or whose `leased` timestamp has expired. Claiming sets
`status` to `leased`, sets `lease_expires_at` to now plus
`lease-seconds`, and increments `attempts` before the sink process is started.
The transaction containing the claim must be committed first.

The worker starts one sink process for each claimed delivery and writes exactly
one UTF-8 JSON object followed by `\n` to its stdin:

```json
{"delivery_id":"del-1","event_type":"release.published","payload":{"version":"0.4.0"}}
```

The worker reads exactly one JSON object line in response. A delivery succeeds
only when the response is an object with the matching `delivery_id` and
boolean `ok: true`, for example:

```json
{"delivery_id":"del-1","ok":true}
```

A response with `ok: false`, a mismatched id, malformed JSON, unexpected EOF,
or a sink failure is a failed attempt. The worker records a non-empty concise
error, clears the lease, leaves the delivery `pending`, continues its bounded
cleanup, and exits non-zero if any attempted delivery failed. A successful
response records the incremented attempt count, changes the row to `delivered`,
and clears the lease and error.

If the worker process dies after the claim transaction commits but before the
delivery is finalized, the row remains `leased` until its timestamp expires.
A later worker must be able to reclaim it and deliver it. The contract is
at-least-once: a sink may observe the same delivery more than once around a
worker crash. Delivered rows are never sent again by later `--once` runs.

The worker exits zero when there are no available deliveries or every attempted
delivery succeeds. It exits non-zero for a delivery failure or invalid worker
invocation. It must not require a network call or third-party package.

## Evaluation

The project succeeds when all of the following are true:

- from `project/`,
  `python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v`
  passes;
- the independent oracle in `../acceptance/test_webhook_inbox.py` passes;
- a fresh process can admit a signed delivery, run a separate sink-backed
  worker, observe durable delivery, kill a worker after a committed lease,
  wait for expiry, reclaim the delivery with a fresh worker, and restart the
  server against the same SQLite file;
- invalid signatures and invalid JSON cannot mutate an accepted delivery;
- the README documents both commands, signature construction, HTTP routes,
  lease/reclaim semantics, sink protocol, persistence, and exact test commands;
- only Python standard-library imports are used.

## Deliberate non-goals

- authentication beyond one shared HMAC secret, authorization, HTTPS, replay
  windows, key rotation, or deployment packaging;
- concurrent worker guarantees, distributed locks, migrations, or multiple
  database backends;
- background lease cleanup, scheduled retries, exponential backoff, or a UI;
- arbitrary sink protocols, shell pipelines, or external network services;
- exactly-once delivery semantics;
- accepting arbitrary JSON fields or silently normalizing an invalid request.
