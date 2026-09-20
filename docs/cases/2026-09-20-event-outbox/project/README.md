# Event Outbox

Event Outbox is a small Python-standard-library example of a durable HTTP
outbox and a separate process worker. The HTTP service records events in
SQLite. A bounded worker later sends pending events to a child sink over one
newline-delimited JSON request/response per event.

## Run the service

From this directory:

```text
python -m outbox serve --db ./var/events.sqlite3 --host 127.0.0.1 --port 8000
```

The database and its parent directory are created on first use. `GET /healthz`
checks liveness. `POST /events` accepts an object with `event_id`, `topic`, and
object `payload`; a repeated identical event id is idempotent. `GET
/events/<event_id>` returns `pending` or `delivered` state, the attempt count,
and the last delivery error.

## Run a worker

The worker processes one pending snapshot and exits:

```text
python -m outbox worker --db ./var/events.sqlite3 --sink python --sink-arg ./sink.py --sink-arg=--log --sink-arg ./var/sink.ndjson --once
```

Each sink request is a JSON line containing `event_id`, `topic`, and `payload`.
The sink must reply with a JSON line such as
`{"event_id":"evt-1","ok":true}`. A negative, malformed, mismatched, or
missing reply leaves the event pending, increments `attempts`, records an
error, and makes the worker exit non-zero. The worker passes `--sink` and every
`--sink-arg` directly to the child process; it never invokes a shell.

## Tests

The project has no third-party dependencies:

```text
python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v
```

The case-level fresh-process HTTP, restart, retry, and independent sink oracle
lives in `../acceptance/test_event_outbox.py`.
