# Live example: Lease Cascade

**Lease Cascade** is the seventh runnable case in this repository. It keeps
the authenticated admission, SQLite state, dependency scheduling, leases,
reclaim, retry handling, and direct-argv sink from [Artifact Pipeline](artifact-pipeline.md),
then adds a bounded fan-out/fan-in barrier. A barrier selects one top-level
output field from each successful direct dependency and passes the values to a
downstream job in declared dependency order.

The [specification](https://github.com/SaehwanPark/rupi/blob/main/docs/cases/2026-09-20-lease-cascade/project/SPEC.md),
[implementation](https://github.com/SaehwanPark/rupi/tree/main/docs/cases/2026-09-20-lease-cascade/project),
and [independent acceptance oracle](https://github.com/SaehwanPark/rupi/blob/main/docs/cases/2026-09-20-lease-cascade/acceptance/test_lease_cascade.py)
are checked into the repository.

The checked-in project suite (7 tests) and fresh-process oracle (5 tests) pass
after bounded tester repair. The local-model turns did not complete the
project; the [case evidence](https://github.com/SaehwanPark/rupi/tree/main/docs/cases/2026-09-20-lease-cascade)
keeps that model stopgate separate from the green fixture.

## 1. Verify the project independently

From the project directory, run its suite and inspect the command contracts:

```bash
cd docs/cases/2026-09-20-lease-cascade/project
python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v
python -m leasecascade --help
python -m leasecascade serve --help
python -m leasecascade worker --help
```

Run the independent oracle from the case root:

```bash
cd ..
python -W error::ResourceWarning -m unittest discover -s acceptance -p "test_*.py" -v
cd project
```

The oracle starts fresh service, worker, and sink processes. It checks signed
atomic admission, idempotency and conflicts, fan-out, ordered barrier fan-in,
declared output references, retryable and terminal failures, blocked cascades,
missing-field local failure, lease reclaim, restart persistence, direct-argv
execution, and the standard-library-only boundary.

## 2. Run the service and worker

Start the service in one terminal:

```bash
python -m leasecascade serve --db state/pipeline.sqlite3 \
  --secret development-secret --host 127.0.0.1 --port 8787
```

`POST /pipelines` accepts ordered jobs with `depends_on`, `input_refs`, and an
optional barrier `collect` declaration. Each input reference names one
top-level field from a successful dependency's JSON `output`. A barrier uses
`{"field":"artifact_id","as":"artifacts"}` to collect that field from
each direct dependency, preserving the declared order. Invalid references,
cycles, malformed input, and conflicts do not leave partial state.

Process available jobs once using the independent sink:

```bash
python -m leasecascade worker --db state/pipeline.sqlite3 \
  --sink python --sink-arg ../acceptance/fake_sink.py \
  --sink-arg=--log --sink-arg=state/delivery.ndjson \
  --lease-seconds 30 --once
```

The worker claims only jobs whose dependencies succeeded and commits each
lease before starting the sink. A retryable failure remains pending for a
later worker; a terminal failure blocks dependents. A barrier with a missing
selected field fails locally without invoking its sink. A killed worker leaves
a lease for a later reclaim, and direct sink arguments are passed as argv
without a shell.

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

- bounded ordered fan-out/fan-in through an explicit barrier;
- selected top-level output propagation without implicit dependency access;
- retry, terminal, blocked, and lease-reclaim state transitions;
- direct-argv process isolation for the sink; and
- read-only trace/replay evidence for bounded model work.
