# Test Ledger

Test Ledger is a small, dependency-free Python 3 task ledger used as rupi's live
example. The implementation is intentionally ordinary: a command-line interface,
human-readable JSON state, and tests that can run from a clean checkout.

## Run it directly

From this directory:

```bash
python -m unittest discover -s tests -p "test_*.py" -v
python -m tasklog add "write the release notes"
python -m tasklog add "run the tests"
python -m tasklog list
python -m tasklog done 1
python -m tasklog list --all
```

The default state file is `.tasklog.json`. Use `python -m tasklog --help` for
the complete command and state-path contract. The independent subprocess oracle
is in `../acceptance/test_tasklog_subprocess.py`.

## Use it with rupi

The adjacent `rupi.config.json` is a local-model example for this disposable
workspace. It enables mutations for the original case study; change
`auto_approve_mutating` to `false` before a read-only first request in a real
workspace.

```bash
rupi run --config rupi.config.json --cwd . \
  --prompt "Read SPEC.md and the project files. Do not change anything. Summarize the contract and name the tests that prove it."
```

After a run, inspect the durable record without contacting the model again:

```bash
rupi trace --config rupi.config.json --quiet --no-reasoning
rupi replay .rupi-state/sessions/<session-id>.trace.jsonl --tools --sequence
```

The complete walkthrough is in the [rupi user manual](https://saehwanpark.github.io/rupi/getting-started/test-ledger.html).
