# Invariant Review

Verdict: pass

## Scope

Reviewed the Round 7 implementation against the canonical runtime, event, session,
recovery, failover, security, startup, and compatibility invariants. The change keeps
provider call labels as protocol metadata while making the durable invocation identity
causal, closes interactive sessions only on trusted paths, and preserves sink-failure
recovery barriers.

## Findings

No blocking findings.

## Checks Confirmed

- `crates/pi-rs-provider/src/decode.rs:193` rejects duplicate provider call IDs in one
  decoded response before emitting any call; IDs may be reused in later responses.
- `crates/pi-rs-runtime/src/turn.rs:2731` parents normal tool lifecycle events as
  completion -> request -> start -> terminal outcome. `crates/pi-rs-store/src/store.rs:3722`
  resolves by causal event identity and only falls back to a unique active provider ID
  for parentless legacy traces.
- `crates/pi-rs-runtime/src/turn.rs:709` uses typed `Refused` management outcomes;
  interactive sink errors propagate while refusals remain recoverable. `src/interactive.rs:1129`
  closes explicit exits, and `src/interactive.rs:888` closes typed fatal turn failures
  without fabricating closure after a sink failure.
- `src/interactive.rs:1200` installs a minimal Windows console handler using
  `CancelToken::raw_flag()` and unregisters it through RAII. The handler performs only
  an atomic flag store; a Windows-target check and callback regression test are present.
- `crates/pi-rs-store/src/store.rs:703` preflights the redacted semantic record before
  recovery blobs, WAL preparation, or canonical append. Recovery blob cleanup retains
  any payload still named by a pending WAL intent.
- `src/interactive.rs:427` restores raw terminal mode from a panic hook even with the
  release profile's `panic = "abort"`; the prior hook is restored on ordinary drop.
- `crates/pi-rs-provider/src/openai.rs:47` uses the shared URL redactor, and
  `crates/pi-rs-provider/src/relay.rs:35` bounds detached resolver work to one worker
  plus one queued request.
- Unknown/mutating tool outcomes remain unchanged and no replay path upgrades an
  uncertain operation to success.

## Verification Evidence

- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- `cargo doc --workspace --no-deps`
- `cargo check -p pi-rs --target x86_64-pc-windows-msvc`
- GitHub Actions run `35463078252`: Ubuntu, macOS, and Windows checks passed.
- Startup benchmark: cold 11.92 ms, warm median 7.23 ms.
- Render benchmark: all four cases within configured budgets.

## Residual Risk

The local environment cannot deliver a real Windows console control event; the
Windows callback is directly unit-tested and the Windows CI job passed. Parentless
legacy traces with multiple simultaneously active identical provider IDs fail closed
as ambiguous, which is deliberate because no causal information exists to choose a
safe invocation.
