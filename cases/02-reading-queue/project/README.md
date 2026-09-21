# Read Queue

Read Queue is a dependency-free Python 3 HTTP JSON service for a small
personal reading queue. It stores items in SQLite and keeps the API deliberately
small.

## Run

From this directory, start the service with:

```text
python -m readqueue serve --db ./var/readqueue.sqlite3 --host 127.0.0.1 --port 8000
```

The database and its parent directory are created on first use. Stop the server
with `Ctrl-C`; starting it again with the same `--db` path restores the items.

## API

- `GET /healthz` — liveness check.
- `GET /items?status=queued|reading|done&tag=TAG` — list items by id.
- `POST /items` — create `{"title": "...", "url": "...", "tags": ["..."]}`.
- `GET /items/<id>` — read one item.
- `PATCH /items/<id>` — update `title`, `url`, `tags`, or `status`.
- `DELETE /items/<id>` — remove one item.

Responses are deterministic JSON. Invalid requests return a JSON `error` and a
4xx status; duplicate URLs return `409`.

## Tests

The project uses only the Python standard library:

```text
python -m unittest discover -s tests -p "test_*.py" -v
python -m readqueue --help
python -m readqueue serve --help
```
