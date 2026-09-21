# Final report: Lease Cascade live case

Status: tester case complete; model-authoring acceptance stopgate open
Date: 2026-09-20  
T: Lease Cascade  
Branch: `tester/2026-09-20-loop-6-lease-cascade`  
Base: `main` at `e450c6c`  
Model: local Qwen-compatible server, `qwen3.8-flash-next`
Draft PR: https://github.com/SaehwanPark/rupi/pull/114

## Result

Lease Cascade is one focused step beyond Artifact Pipeline: it retains signed
atomic admission, SQLite persistence, leases/reclaim, direct-argv delivery,
and declared output references, then adds an explicit `barrier` job that
selects one top-level output field from each successful direct dependency and
delivers an ordered fan-in collection. Missing selected output fails the
barrier locally without invoking its sink and blocks downstream work.

The independent acceptance gates pass after a tester repair:

| Gate | Command/result |
| --- | --- |
| Project suite | `python -W error::ResourceWarning -m unittest discover -s tests -p 'test_*.py' -v`; `7` tests, exit `0`, final wrapper `579 ms` |
| Fresh-process oracle | `python -W error::ResourceWarning -m unittest discover -s acceptance -p 'test_*.py' -v`; `5` tests, exit `0`, final wrapper `7,298 ms` |

The oracle imports only Python standard-library modules. It starts fresh service,
worker, and sink processes and independently checks authentication, atomic
admission/idempotency/conflict, fan-out and ordered fan-in, selected output
references, retry/terminal/blocked behavior, missing-field local failure,
crash-lease reclaim, restart persistence, help output, and the standard-library
boundary.

These passing gates are not model-completion evidence.

## Model attempts and stopgates

The exact prompts are committed under `prompts/` and are recorded with hashes
and invocation details in [`OBSERVATIONS.md`](OBSERVATIONS.md).

- I-01, session `01a0c0ea-77bf-7b0a-8be4-cb0242fbbccd`: three requests, exit
  `1`, `218,297 ms`. Qwen read the specification, created only
  `leasecascade/__init__.py` and `__main__.py`, then hit the configured
  `120,000 ms` provider timeout.
- R-01, session `01a0c0ee-e055-7674-8d7c-2c806be0cceb`: two requests, exit
  `1`, `128,819 ms`. It inspected the partial workspace and hit the same
  configured provider timeout without completing another file.
- The setup-fidelity session with an incorrect prompt path and the two bounded
  read-only verification sessions are excluded from authoring evidence.

The model-authoring stopgate therefore remains open. No model summary is used
to claim completion.

## Tester repair boundary

The tester completed the project only inside this case directory. The retained
model placeholder was revised and the tester added `cli.py`, `ids.py`,
`storage.py`, `service.py`, `worker.py`, `README.md`, and focused project
tests. One local SQL projection bug was found by the project suite and fixed by
adding `lease_expires_at` to the selected columns. The acceptance oracle and
fake sink were not changed after the baseline.

No rupi runtime source, runtime tests, parent integration document, roadmap,
or canonical design file was changed.

## Trace, replay, and invariants

`rupi trace` and `rupi replay --tools --sequence` exited `0` for all five
material sessions. Replay emitted only recorded history; it did not invoke
the provider or execute recorded tools. The traces show one active local
model, native reasoning provenance, and no backup activation or model
orchestration.

The case keeps the relevant safeguards explicit: durable state controls sink
side effects, direct argv is required, failed/uncertain work is not silently
counted as success, and replay is never used to apply changes. Since no rupi
runtime code changed, invariant review found no new runtime finding.

## Repository verification

The following checks passed: `cargo fmt --all --check`,
`cargo check -p rupi-core --all-features`,
`cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace --quiet`, and `cargo doc --workspace --no-deps`.

The startup benchmark passed when run in a login WSL shell:
`bash -lc './bench/startup.sh --json /tmp/rupi-loop6-startup.json'` reported
cold `12.59 ms`, warm mean `11.29 ms`, warm median `11.22 ms`, and max
`12.08 ms`. A direct non-login `bash bench/startup.sh ...` attempt failed only
because that shell did not expose `cargo`; the corrected command is recorded
in the observations.

## Residual risk and recommended action

The evidence demonstrates the project contract through independent tester
artifacts, not through successful model construction. Treat this as a
tester-repaired compatibility/acceptance result and keep the model-authoring
stopgate visible in Loop 6 synthesis. Recommended action: parent reviews the
case and decides whether the provider timeout and Windows shell friction merit
a follow-up harness/configuration slice; do not weaken rupi semantics or
count this as model completion.

The PR is intentionally still draft and unmerged. Parent integration docs
remain untouched.
