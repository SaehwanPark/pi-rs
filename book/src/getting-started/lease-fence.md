# Live example: Lease Fence

**Lease Fence** is the eighth runnable case in this repository. It keeps the
authenticated admission, SQLite state, dependency scheduling, ordered barrier
fan-in, leases, reclaim, retry handling, and direct-argv sink from [Lease
Cascade](lease-cascade.md), then adds one focused reliability rule: a stale
worker response cannot overwrite a newer reclaimed claim.

The [specification](https://github.com/SaehwanPark/rupi/blob/main/docs/cases/2026-09-20-lease-fence/project/SPEC.md),
[implementation](https://github.com/SaehwanPark/rupi/tree/main/docs/cases/2026-09-20-lease-fence/project),
and [independent acceptance oracle](https://github.com/SaehwanPark/rupi/blob/main/docs/cases/2026-09-20-lease-fence/acceptance/test_lease_fence.py)
are checked into the repository.

The checked-in project suite (7 tests) and fresh-process oracle (4 tests) pass
after bounded tester repair. The local-model turns did not complete the
project; the [case evidence](https://github.com/SaehwanPark/rupi/tree/main/docs/cases/2026-09-20-lease-fence)
keeps that model stopgate separate from the green fixture.

## 1. Verify the project independently

From the project directory, run its suite and inspect the command contracts:

```bash
cd docs/cases/2026-09-20-lease-fence/project
python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v
python -m leasefence --help
python -m leasefence serve --help
python -m leasefence worker --help
```

Run the independent oracle from the case root:

```bash
cd ..
python -W error::ResourceWarning -m unittest discover -s acceptance -p "test_*.py" -v
cd project
```

The oracle starts separate service, worker, and sink processes. It checks
authenticated atomic admission, idempotency and conflicts, ordered fan-out and
barrier fan-in, declared output references, retryable and terminal failures,
blocked dependents, restart persistence, lease reclaim, stale-worker fencing,
direct-argv execution, help output, and the standard-library-only boundary.

## 2. Run the service and worker

Start the service in one terminal:

```bash
python -m leasefence serve --db state/pipeline.sqlite3 \
  --secret development-secret --host 127.0.0.1 --port 8787
```

`POST /pipelines` accepts the signed pipeline contract from `SPEC.md`.
`input_refs` copy only explicitly named top-level output fields, and a
`barrier` collects one selected field from each direct dependency in declared
order. Invalid signatures, cycles, malformed input, and conflicting pipeline
ids do not leave partial state.

Process available jobs once using a direct-argv sink:

```bash
python -m leasefence worker --db state/pipeline.sqlite3 \
  --sink python --sink-arg ../acceptance/fake_sink.py \
  --sink-arg=--log --sink-arg=state/delivery.ndjson \
  --lease-seconds 30 --once
```

Each claim receives a private fencing token. If the lease expires, a fresh
worker clears that token and claims the job with a new one. The old worker may
still return later, but finalization requires the current token and leased
status; the stale worker exits non-zero and the newer result remains durable.
This prevents stale completion from overwriting state, but does not promise
exactly-once external side effects.

Retryable failures remain pending for a later worker, terminal failures block
dependents, and a killed worker can be reclaimed after expiry. The worker is
bounded, does not poll, and never invokes the sink through a shell.

## 3. Ask rupi for a bounded verification

Keep implementation and independent acceptance in separate turns. A timeout or
request-budget exhaustion is incomplete work, not evidence of a passing
project. For a read-only review, use the separate verification config and ask
the model to run only the project suite:

```bash
rupi run --config rupi.verify.config.json --cwd . \
  --prompt "Read SPEC.md and inspect the implementation briefly. Run the project suite in one bounded slice, report exact results, and do not run the independent acceptance oracle."
```

Use a disposable copy or version-controlled branch before enabling mutations
with the implementation config.

## 4. Inspect and replay the evidence

After a run, inspect the durable trace without contacting the model or
executing old tools:

```bash
rupi trace --config rupi.verify.config.json --quiet --no-reasoning
rupi replay .rupi-state-verify/sessions/<session-id>.trace.jsonl --tools --sequence
```

Only call a new implementation complete after both the project suite and the
independent fresh-process oracle pass. This case demonstrates:

- private per-claim fencing for stale-worker rejection;
- durable lease reclaim without stale result overwrite;
- ordered barrier fan-in and explicitly declared data flow;
- retry, terminal, blocked, and direct-argv worker behavior; and
- read-only trace/replay evidence for bounded model work.
