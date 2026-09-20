# Live example: Event Outbox

**Event Outbox** is the third runnable case in this repository. It keeps the
HTTP, SQLite, and restart boundaries from [Read Queue](read-queue.md), then adds
idempotent event admission, retryable delivery state, and a separate sink
process connected by a newline-delimited JSON protocol.

The [specification](https://github.com/SaehwanPark/rupi/blob/main/docs/cases/2026-09-20-event-outbox/project/SPEC.md),
[implementation](https://github.com/SaehwanPark/rupi/tree/main/docs/cases/2026-09-20-event-outbox/project),
and [independent acceptance oracle](https://github.com/SaehwanPark/rupi/blob/main/docs/cases/2026-09-20-event-outbox/acceptance/test_event_outbox.py)
are checked into the repository.

## 1. Verify the project independently

From a clone of rupi, run the project tests and inspect the command contracts:

```bash
cd docs/cases/2026-09-20-event-outbox/project
python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v
python -m outbox --help
python -m outbox serve --help
python -m outbox worker --help
```

Then run the fresh-process oracle from the case root, not from inside the model
workspace:

```bash
cd ..
python -W error::ResourceWarning -m unittest discover -s acceptance -p "test_*.py" -v
cd project
```

The oracle starts and restarts the HTTP service, checks idempotency and invalid
input, runs the worker against an independent sink process, and verifies that a
failed delivery remains pending and retryable.

## 2. Run the service and worker

The service creates its SQLite database directory on first use:

```bash
python -m outbox serve --db ./var/events.sqlite3 --host 127.0.0.1 --port 8000
```

In another terminal, the worker processes one pending snapshot and exits. The
case oracle supplies the independent sink at `../acceptance/fake_sink.py`:

```bash
python -m outbox worker --db ./var/events.sqlite3 \
  --sink python \
  --sink-arg ../acceptance/fake_sink.py \
  --sink-arg=--log --sink-arg ./var/sink.ndjson \
  --once
```

The worker passes the sink and every `--sink-arg` directly to the child process;
shell metacharacters are data, not commands. A successful sink response marks an
event `delivered`. A negative, malformed, or missing response increments
`attempts`, records an error, and leaves the event `pending` for a later `--once`
run.

## 3. Ask rupi for a bounded verification

Keep implementation and independent acceptance in separate turns. For a
read-only verification, copy one of the case configs and disable mutating
approval first. Use a request deadline and a modest request budget when running
against a local model; a timeout is an incomplete turn, not evidence that the
project passed.

```bash
rupi run --config rupi.recovery.config.json --cwd . \
  --prompt "Read SPEC.md and inspect the implementation briefly. Run the project suite in one bounded slice, report exact results, and do not run the independent acceptance oracle."
```

On Windows PowerShell, use direct argv process execution for known programs and
`dir` rather than Unix `ls` when shell listing is needed. This keeps arguments
unambiguous across the service, worker, and sink process boundary.

## 4. Inspect and replay the evidence

After the run, inspect the trace without contacting the model or executing old
tools:

```bash
rupi trace --config rupi.recovery.config.json --quiet --no-reasoning
rupi replay .rupi-state/sessions/<session-id>.trace.jsonl --tools --sequence
```

Call the project complete only after both the project suite and the independent
fresh-process oracle pass. This case demonstrates:

- durable HTTP admission with idempotency and conflict detection;
- SQLite state that survives service restart;
- at-least-once delivery with explicit attempts and retry state;
- direct-argv process isolation for a line-delimited sink protocol;
- trace and replay evidence for a bounded, recoverable workflow.
