# Observations log

This is the chronological evidence log for the first-user test. The final
classification and recommendations are in `FINAL_REPORT.md`.

## Starting conditions

- Repository branch: `test/first-user-task-ledger`
- Existing unrelated worktree item: `.pi/.goals-pool-snapshot.json` (left untouched)
- Endpoint: `http://127.0.0.1:8000/v1`
- Model alias: `qwen3.8-flash-next`
- Harness source invocation: `cargo run --quiet -- ...` from the `rupi` checkout
- Mutation policy for the isolated toy workspace: enabled in
  `toy-project/rupi.config.json`

## Observation entry template

For each interaction, record:

```text
### <id> — <short title>
Condition:
Command/prompt:
Expected:
Observed:
Exit/status:
Evidence:
Classification:
Impact:
```

## Live log

The entries below will be appended as the case runs. No `rupi` issue is being
fixed during this session.

