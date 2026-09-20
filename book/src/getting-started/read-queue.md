# Live example: Read Queue

**Read Queue** is the second runnable case in this repository. It is a
dependency-free Python 3 HTTP/JSON service backed by SQLite. Compared with
[Test Ledger](test-ledger.md), it adds a long-lived process, HTTP boundaries,
relational persistence, and recovery after a server restart.

The [specification](https://github.com/SaehwanPark/rupi/blob/main/docs/cases/2026-09-20-reading-queue/project/SPEC.md),
[implementation](https://github.com/SaehwanPark/rupi/tree/main/docs/cases/2026-09-20-reading-queue/project),
and [independent acceptance oracle](https://github.com/SaehwanPark/rupi/blob/main/docs/cases/2026-09-20-reading-queue/acceptance/test_readqueue_http.py)
are checked into the repository.

## 1. Verify the project independently

From a clone of rupi, enter the project directory and run its own tests before
asking a model to inspect it:

```bash
cd docs/cases/2026-09-20-reading-queue/project
python -W error::ResourceWarning -m unittest discover -s tests -p "test_*.py" -v
python -m readqueue --help
python -m readqueue serve --help
```

The independent oracle must be run from the case root, not from inside the
model workspace:

```bash
cd ..
python -W error::ResourceWarning -m unittest discover -s acceptance -p "test_*.py" -v
cd project
```

It starts the service in a fresh process, exercises CRUD, filtering and error
responses, then restarts the service against the same SQLite database.

## 2. Ask rupi for a bounded verification

The checked-in configs target the local Qwen endpoint and enable mutations for
the original case study. For a read-only review, copy one config and set
`tools.auto_approve_mutating` to `false` first. Keep implementation and
verification in separate turns; a request-budget exhaustion or provider request
deadline is an incomplete turn, not evidence that the project passed. For an
implementation slice, use a copied config with a bounded `request_timeout_ms`
(for example `120000`) and a modest output cap; inspect and resume after a timeout
instead of letting one local-model request consume the whole session.

```bash
rupi run --config rupi.recovery.config.json --cwd . \
  --prompt "Read SPEC.md and inspect the implementation briefly. Work in a small bounded slice. Run the project suite before broad polish, then report exact command results. Do not run the independent acceptance oracle; I will run it separately."
```

On Windows PowerShell, use the direct argv `process` tool for known programs
(`python.exe` with an argument array) and use `dir` rather than Unix `ls` when
shell listing is needed. This avoids shell quoting differences and keeps the
program arguments unambiguous.

## 3. Inspect the evidence

After the run, inspect the session without contacting the model or executing
old tools:

```bash
rupi trace --config rupi.recovery.config.json --quiet --no-reasoning
rupi replay .rupi-state/sessions/<session-id>.trace.jsonl --tools --sequence
```

Only call the project complete after both its own suite and the independent
fresh-process oracle pass. The case demonstrates a useful rupi workflow:

- a multi-file service can be built from a written contract;
- direct process execution makes platform-specific verification explicit;
- restart behavior is checked outside the model's claims;
- trace and replay preserve what happened when a bounded turn is interrupted.
