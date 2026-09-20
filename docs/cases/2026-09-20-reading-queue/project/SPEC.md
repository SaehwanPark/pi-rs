# Read Queue project specification

## Goal

Build `readqueue`, a dependency-free Python 3 HTTP JSON service for a small
personal reading queue. It should be a readable multi-file project that can be
started from a clean checkout, tested without third-party packages, and resumed
against the same SQLite database after a server restart.

## Command

```text
python -m readqueue serve --db PATH --host HOST --port PORT
```

The server must bind the requested host and port, print a short startup message
including the address, and keep serving until the process is stopped. The
database and parent directory are created when needed.

## HTTP contract

All successful responses are JSON and use `Content-Type: application/json`.
Object keys and list ordering must be deterministic. Error responses are JSON
objects with an `error` string and use a suitable 4xx status.

### `GET /healthz`

Return `200` and `{"ok": true}` without requiring any items.

### `GET /items`

Return `200` and a JSON array of items ordered by ascending numeric `id`.
Optional query filters are `status` (`queued`, `reading`, or `done`) and `tag`
(an exact case-insensitive tag match).

### `GET /items/<id>`

Return one item with `200`, or a JSON 404 error for an unknown positive integer
id.

### `POST /items`

Accept a JSON object with:

```json
{"title": "Read the design", "url": "https://example.test/design", "tags": ["rupi", "docs"]}
```

`title` and `url` are required non-empty strings. `tags` is optional and must be
a list of non-empty strings; stored tags are trimmed, lower-case, unique, and
sorted. New items start with status `queued`. Return `201` and the complete
created item. A malformed JSON document, wrong field type, missing required
field, unknown field, or invalid tag returns `400` without changing the DB.
Duplicate URLs return `409` without changing existing data.

### `PATCH /items/<id>`

Accept a non-empty JSON object containing one or more of `title`, `url`, `tags`,
and `status`. The same field rules apply; status must be `queued`, `reading`, or
`done`. Return `200` and the complete updated item. Unknown ids return `404`;
invalid input returns `400`; a URL collision returns `409`; all failures leave
the stored item unchanged.

### `DELETE /items/<id>`

Delete an existing item and return `204` with no response body. An unknown id
returns a JSON 404 error and does not change other items.

Other paths or methods return a JSON 404/405 error as appropriate.

## Evaluation

The project succeeds when all of the following are true:

- `python -m unittest discover -s tests -p "test_*.py" -v` passes from the
  project root;
- an independent fresh-process acceptance check can start the server, create
  two items, filter/list/read them, patch one, delete one, and restart the
  server against the same DB;
- malformed JSON, missing/unknown fields, invalid status/tag values, duplicate
  URLs, and unknown ids return non-success statuses with an `error` field and
  preserve prior data;
- `python -m readqueue --help` and `python -m readqueue serve --help` explain
  how to start the service;
- only the Python standard library is imported.

## Deliberate non-goals

- authentication, HTTPS, deployment, migrations, or multiple database backends;
- a browser UI, CLI client, background worker, or external network dependency;
- optimistic locking or a promise of concurrent-write isolation;
- support for arbitrary JSON fields beyond the documented contract.

