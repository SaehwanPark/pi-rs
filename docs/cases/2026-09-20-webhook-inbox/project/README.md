# Webhook Inbox

`webhookinbox` is a small Python 3 standard-library service for accepting
authenticated webhook deliveries into SQLite and sending them through a
separate bounded worker. It is an educational at-least-once delivery example,
not a production gateway.

## Run the service

From this directory:

```text
python -m webhookinbox serve --db var/inbox.sqlite3 --secret oracle-secret --host 127.0.0.1 --port 8080
```

The service creates the database and parent directory. The secret is used only
to verify incoming requests and is not stored in the database.

## Signed admission

`POST /deliveries` requires the exact raw UTF-8 body and this header:

```text
X-Webhook-Signature: sha256=<hex HMAC-SHA256>
```

The digest is `hmac.new(secret.encode("utf-8"), body, hashlib.sha256).hexdigest()`.
The JSON body has exactly these fields:

```json
{"delivery_id":"del-1","event_type":"release.published","payload":{"version":"0.4.0"}}
```

The first delivery returns `202`; repeating the same id and content returns
`200`. Reusing an id for different content returns `409`. Bad signatures return
`401`, and invalid signed JSON returns `400` without changing stored data.

The service also provides:

- `GET /healthz` — `{"ok": true}`;
- `GET /deliveries/<delivery_id>` — status, attempts, errors, and lease state.

Delivery state is `pending`, `leased`, or `delivered`. A worker commits a lease
and increments `attempts` before starting its sink. If the worker dies, the
lease expires and a later worker can reclaim the row. This is at-least-once
delivery: a sink may observe a delivery again around a crash.

## Run the bounded worker

```text
python -m webhookinbox worker --db var/inbox.sqlite3 --sink python --sink-arg sink.py --sink-arg=--tag=local --lease-seconds 30 --once
```

The worker starts the sink with direct argv, never a shell. It sends one JSON
line per claimed delivery and expects one response line containing the matching
`delivery_id` and `"ok": true`:

```json
{"delivery_id":"del-1","event_type":"release.published","payload":{"version":"0.4.0"}}
{"delivery_id":"del-1","ok":true}
```

Failed sink responses leave a delivery pending with a concise error and make
the worker exit non-zero. `--once` never waits for new deliveries.

## Checks

The project has no third-party dependencies:

```text
python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v
python -m webhookinbox --help
python -m webhookinbox serve --help
python -m webhookinbox worker --help
```

The case-level fresh-process acceptance oracle is outside this project at
`../acceptance/test_webhook_inbox.py`.
