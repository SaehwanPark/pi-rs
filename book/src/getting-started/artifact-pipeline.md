# Live example: Artifact Pipeline

**Artifact Pipeline** is the sixth runnable case in this repository. It keeps
the authenticated admission, SQLite state, dependency scheduling, leases,
reclaim, retry handling, and direct-argv sink from [Batch Relay](batch-relay.md),
then adds explicit output-to-input data flow between successful jobs.

The [specification](https://github.com/SaehwanPark/rupi/blob/main/docs/cases/2026-09-20-artifact-pipeline/project/SPEC.md),
[implementation](https://github.com/SaehwanPark/rupi/tree/main/docs/cases/2026-09-20-artifact-pipeline/project),
and [independent acceptance oracle](https://github.com/SaehwanPark/rupi/blob/main/docs/cases/2026-09-20-artifact-pipeline/acceptance/test_artifact_pipeline.py)
are checked into the repository.

The checked-in project suite (6 tests) and fresh-process oracle (4 tests) pass
after bounded tester repair. The local-model turns did not complete the
project; the [case evidence](https://github.com/SaehwanPark/rupi/tree/main/docs/cases/2026-09-20-artifact-pipeline)
keeps that model stopgate separate from the green fixture.

## 1. Verify the project independently

From the project directory, run its suite and command help:

```bash
cd docs/cases/2026-09-20-artifact-pipeline/project
python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v
python -m artifactpipe --help
python -m artifactpipe serve --help
python -m artifactpipe worker --help
```

Run the independent oracle from the case root:

```bash
cd ..
python -W error::ResourceWarning -m unittest discover -s acceptance -p "test_*.py" -v
cd project
```

The oracle starts fresh service, worker, and sink processes. It checks signed
admission, atomic idempotency and conflicts, declared references, dependency
order, output propagation, retryable and terminal failures, blocked jobs,
lease reclaim, direct-argv execution, restart persistence, and the
standard-library-only boundary.

## 2. Run the service and worker

Start the service in one terminal:

```bash
python -m artifactpipe serve --db state/pipeline.sqlite3 --secret development-secret --host 127.0.0.1 --port 8787
```

`POST /pipelines` accepts ordered jobs with `depends_on` and `input_refs`.
Each input reference names one top-level field from a successful dependency's
JSON `output`, and that dependency must be declared explicitly. The
`X-Pipeline-Signature` header is `sha256=<hex digest>`, where the digest is the
HMAC-SHA256 of the exact raw UTF-8 body using the server secret as the UTF-8
key. Invalid references, cycles, malformed input, and conflicts do not leave
partial state.

Run one bounded worker pass with the independent sink:

```bash
python -m artifactpipe worker --db state/pipeline.sqlite3 \
  --sink python --sink-arg ../acceptance/fake_sink.py \
  --sink-arg=--log --sink-arg=state/delivery.ndjson \
  --lease-seconds 30 --once
```

The sink receives each job's resolved `inputs` and returns an object-valued
`output`. Successful outputs are retained in SQLite for explicitly declared
downstream references. Retryable failures remain pending for a later worker;
terminal failures block dependents. A killed worker leaves a lease for a later
reclaim, and direct sink arguments are passed as argv without a shell.

## 3. Ask rupi for a bounded verification

Keep implementation and acceptance in separate turns. A timeout or exhausted
request budget is incomplete work, not evidence of a passing pipeline. For a
read-only review, use the verification config and run only the project suite:

```bash
rupi run --config rupi.verify.config.json --cwd . \
  --prompt "Read SPEC.md and inspect the implementation briefly. Run the project suite in one bounded slice, report exact results, and do not run the independent acceptance oracle."
```

Use a disposable copy or a version-controlled branch before enabling mutations
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

- atomic pipeline admission with declared, top-level data references;
- durable output propagation without implicit dependency access;
- retry, terminal, blocked, and lease-reclaim state transitions;
- direct-argv process isolation for the sink; and
- read-only trace/replay evidence for bounded model work.
