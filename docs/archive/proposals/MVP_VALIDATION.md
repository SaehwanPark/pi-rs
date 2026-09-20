# MVP validation snapshot (2026-09-14)

This is a machine-specific pre-merge validation snapshot for the MVP documentation
reconciliation. Benchmark numbers are evidence from the scripts on the contributing machine,
not CI budgets or cross-machine performance guarantees.

## Checks

- `cargo fmt --all --check` — passed.
- `cargo check -p rupi-core --all-features` — passed.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — passed, 0 warnings.
- `cargo test --workspace --all-features` — passed, 955 tests.
- `cargo doc --workspace --no-deps` — passed, 0 warnings.
- `git diff --check` — passed.

## Benchmarks

- `bash bench/startup.sh --json bench/results/startup-ci.json` — passed; cold 222.06 ms,
  warm median 3.02 ms.
- `bash bench/cold_start.sh --json bench/results/cold-start-ci.json` — passed; cold median
  162.69 ms, warm median 7.13 ms.
- `bash bench/render.sh` — passed all budgets.
- `bash bench/keystroke.sh` — passed all budgets.
- `bash bench/large_session.sh` — passed all budgets.

These scripts are local pre-merge gates. Their JSON outputs are intentionally ignored because
machine-specific timings are not portable CI artifacts.

## Explicit limits

- `bash bench/warm_start.sh --json bench/results/warm-start-ci.json` skipped cleanly because
  no provider config was present at `~/.config/rupi/config.json`.
- A live local endpoint probe at `127.0.0.1:8080/v1/models` was unavailable. Provider-slice
  tests cover live SSE parsing and tool-schema serialization; the runtime tool loop remains
  covered by scripted providers and the fake OpenAI server integration test.
- The generic `verify_code` helper selected an inapplicable Go profile and failed on
  `go test ./...`; direct Rust checks above are authoritative for this Rust workspace.
