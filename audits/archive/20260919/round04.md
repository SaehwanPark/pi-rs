# Round 4 audit

I audited **PR #93 (`fix/audit-round03`) at `c92e9f4fe7eb192cd03ab819fe6692db2cb41bbf`**, because `main` is still at the Round 2 commit `a0662b1`.

The Round 3 work is substantial. I consider the original six Round 3 findings largely addressed in design terms. However, the fixes introduced several second-order problems, including one already reproduced by CI. My Round 4 verdict is therefore still **fix required**.

**No P0/critical issue found. I have five P1/high-concern findings.**

## Round 3 findings: disposition

| Round 3 finding                                     | Round 4 status                                                                                                                           |
| --------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------- |
| `exec` truncation kills command and reports success | **Closed.** Output is now discarded after the capture ceiling while the process continues; the real exit status decides success/failure. |
| Trace/session projection not crash-atomic           | **Substantially closed.** The new WAL is a meaningful architecture-level fix and generally repairs or fails closed.                      |
| In-flight mutating tools not reconciled             | **Closed.** Started calls are reconstructed from canonical lifecycle state and reconciled before provider contact.                       |
| L0 eviction not resumable                           | **Closed.** A durable `SessionReductionRecord` now carries the model-visible reduction.                                                  |
| HTTP cancellation/deadlines broken                  | **Original issue closed**, but the new cancellation relay introduces a serious DNS/network-policy problem below.                         |
| No exclusive session ownership                      | **Concurrent-writer issue closed**, but the lease implementation itself has a serious crash-reclamation flaw below.                      |

The dynamic registry race, invalid `cwd` fail-open, MCP `roots` advertisement, and provider agent-cache issue from the Round 3 P2 set also appear fixed.

---

## P1-1 — The new WAL currently breaks normal execution on Windows

This one is already reproduced by the repository's own CI.

The current PR's GitHub Actions run passes Ubuntu and macOS, but **Windows fails five ordinary runtime tests** with:

```text
durable sink failure: storage i/o: Access is denied. (os error 5)
```

The failures include ordinary second turns, cancellation, failover, and checkpoint resume—not an exotic WAL-specific unit test.

The source is fairly direct.

`ProjectionWal::commit()` compacts a clean WAL after committing a transaction. On Windows:

```rust
#[cfg(windows)]
fn compact(&mut self) -> Result<(), StoreError> {
  self.writer.clear()
}
```

`LineWriter::clear()` does:

```rust
self.file.set_len(0)?;
```

But `LineWriter::create()` opens that file with:

```rust
OpenOptions::new()
  .append(true)
  .create(true)
  .open(path)?
```

The Windows handle does not have the rights required for the truncate operation, producing the observed `Access is denied`.

### Why P1

This is not merely "Windows CI isn't clean." The new WAL is on normal semantic write paths. As currently implemented, ordinary Windows agent sessions fail with durable-sink errors.

### Recommended fix

I would make WAL compaction independent of an append-only handle. Since compaction only happens after `pending().is_empty()`:

```text
commit is durable
→ close/drop append writer
→ reopen WAL with write+truncate
→ sync
→ reopen append writer
```

Alternatively, ensure the writer is opened with the precise write rights needed for both append and `set_len`, but I prefer separating append and destructive-compaction handles because the intent is clearer.

Add a direct test of:

```text
prepare → canonical/projection commit → WAL compact → next transaction
```

on Windows, rather than relying only on high-level tests to catch it.

---

## P1-2 — Session leases become permanently stale after a crash on macOS and Windows

The lease solves the original concurrent-resume problem, but its recovery strategy is not actually cross-platform.

`lease.rs` checks whether the PID in the owner marker is alive. Linux does:

```rust
fs::metadata(format!("/proc/{pid}")).is_ok()
```

But every non-Linux platform does:

```rust
#[cfg(not(target_os = "linux"))]
fn process_alive(_pid: u32) -> bool {
  true
}
```

Consequently:

```text
macOS/Windows process acquires session lease
→ process crashes / is SIGKILLed / machine reboots
→ lease directory remains
→ every later process treats its dead owner as alive forever
→ --resume permanently refuses the session
```

The same issue can permanently block retention because retention itself uses `SessionLease`.

There is an even narrower cross-platform dead zone during lease acquisition:

```text
create_dir(lease)
↓ process dies here
write/rename owner marker
```

If the owner file is absent or unreadable, `owner_alive()` conservatively returns `true`. Thus a crash between directory creation and owner publication can leave an unreclaimable lease even on Linux. An I/O error while writing the owner marker has similar behavior because the newly created lock directory is not rolled back.

### Recommended fix

I would replace PID-directory liveness with a **kernel-backed advisory file lock**.

The desired semantics are exactly what OS locks already provide:

```text
open <state>/leases/<session>.lock
try exclusive lock
hold File for Session lifetime
OS releases lock automatically on process death
retention tries the same lock before deletion
```

On Unix this maps naturally to `flock`/`fcntl`; on Windows to `LockFileEx`. A small cross-platform crate is preferable to maintaining PID liveness yourself.

The PID/token can remain in the file for diagnostics, but it should not be the mutual-exclusion primitive.

Test with a child process that acquires a lease and is forcibly terminated; the parent must then immediately be able to acquire it on Linux, macOS, and Windows.

---

## P1-3 — The cancellation relay bypasses system DNS and can leak/break private hostnames

This is the most concerning new architectural side effect of the HTTP fix.

Both:

```text
crates/rupi-provider/src/relay.rs
crates/rupi-mcp/src/relay.rs
```

implement their own DNS resolver.

The relay resolves a hostname by:

```text
literal IP
→ hosts file
→ manually issue UDP A/AAAA DNS queries
```

On Unix it derives DNS servers from `/etc/resolv.conf`.

On **Windows**, there is no equivalent resolver discovery. If no address came from the hosts file:

```rust
if addresses.is_empty() {
  addresses.push(1.1.1.1:53);
}
```

So a normal Windows configuration such as:

```text
https://internal-llm.corp.example/v1
```

can result in `internal-llm.corp.example` being sent directly to Cloudflare's public resolver rather than the Windows/VPN/corporate resolver.

This has two consequences:

**Functional:** private DNS, VPN split DNS, Active Directory DNS, enterprise service discovery, and environments that block outbound port 53 can stop working.

**Privacy/policy:** internal provider and MCP hostnames can be disclosed to an external DNS service despite the machine being configured to resolve them privately.

macOS is also problematic. `/etc/resolv.conf` is not a complete representation of macOS's dynamic/scoped resolver state, particularly VPN/split-DNS configurations. The relay therefore does not preserve the network semantics that the original HTTP client inherited from the OS.

The problem extends to proxies: a private corporate proxy hostname itself must first pass through this custom resolver.

### Recommended fix

I would **not continue growing the custom HTTP relay into a DNS/proxy/TLS stack**.

The Round 3 requirement is:

> cancel one in-flight request without silently issuing another POST.

That is naturally supported by a cancellable HTTP client. A lazily initialized async transport would preserve startup goals while delegating:

```text
DNS
TLS
HTTP proxies
NO_PROXY
IPv4/IPv6 selection
connection cancellation
```

to established implementations.

The current relay has already accumulated custom DNS, CONNECT handling, proxy authentication, `NO_PROXY`, origin-form rewriting, TCP tunneling, and duplicated provider/MCP implementations. That surface is becoming materially riskier than introducing a well-contained network dependency.

At minimum, if the relay remains, it must use platform-native resolver semantics. **Hardcoded public DNS fallback should be removed entirely.**

Tests should include a private hostname resolvable only by an injected/system test resolver and should verify that no public DNS endpoint is contacted.

---

## P1-4 — Existing v1 sessions can be resumed and then written into a format the next launch rejects

This is a schema-migration bug and is particularly relevant because the current `main` creates schema-v1 sessions.

The PR moves:

```rust
SESSION_SCHEMA_VERSION
```

to `3`, and introduces `SessionRecord::Reduction` in version 2.

The reader explicitly rejects:

```text
header.version < 2
+
contains SessionRecord::Reduction
```

which is sensible.

But `SessionLog::resume_with_policy()` leaves an existing v1 header unchanged, and `SessionLog::append()` does not prevent a modern `Reduction` record from being appended to that v1 file.

Therefore:

```text
existing main-era session has header version 1
→ upgrade to PR #93
→ --resume succeeds
→ context pressure triggers L0 eviction
→ runtime appends SessionRecord::Reduction
→ current process continues successfully
→ exit
→ next --resume reads header version 1 + reduction record
→ reader rejects the session as invalid
```

In other words, the writer can create a state that its own reader deliberately declares incompatible.

This will affect real pre-upgrade sessions, not merely hand-crafted legacy fixtures.

There is a broader schema-contract smell here as well: newer checkpoint fields can be appended while the header continues claiming an older schema, even where serde compatibility happens to make that readable.

### Recommended fix

Choose one explicit upgrade policy before opening the append handle.

I recommend atomic migration:

```text
acquire session lease
→ read + validate entire old semantic log
→ transform to current schema
→ write <session>.jsonl.new
→ fsync file
→ atomically replace original
→ fsync directory where applicable
→ only then open runtime appenders
```

Alternatively, keep a v1 writer in strict v1 compatibility mode, but that becomes increasingly complicated as session semantics evolve.

Regression test:

```text
create genuine v1 session
→ resume under v3
→ perform L0 reduction
→ close
→ resume again
→ assert model-visible context is identical and header is current schema
```

I would also test v1 → checkpoint → resume and v1 → model failover → reduction → resume.

---

## P1-5 — A normal process crash during model generation still makes the session permanently non-resumable

The new recovery machinery is deliberately conservative, which is generally the right direction. But it currently treats some **provably recoverable interruption states** as permanent corruption.

`validate_model_request_lifecycles()` rejects any canonical:

```text
ModelRequestStarted
...
<process dies>
```

that has no matching `ModelRequestCompleted`.

This is exactly what a normal kill, power loss, OOM, harness crash, or terminal closure while a model is generating will produce.

Nothing unsafe needs to be replayed here. The request may have reached the provider and may have consumed compute, so **do not automatically retry it**. But rupi can safely record that the request was abandoned and allow the user to start a new turn.

Today it instead makes `--resume` fail indefinitely.

There is a similar but even more clearly recoverable tool case. `interrupted_tool_calls()` treats:

```text
ToolRequested
<crash before ToolStarted>
```

as a hard recovery failure.

But the durable execution contract is specifically:

```text
ToolStarted is written before tool code executes.
```

Therefore absence of `ToolStarted` proves that the tool never crossed the execution boundary. This case can safely be normalized to "not executed/interrupted before start" and given a corresponding tool-result message so provider protocol history remains valid.

The current handling of:

```text
ToolRequested → ToolStarted → crash
```

is much better: mutating work is reconciled and ambiguous state blocks continuation. I would preserve that exactly.

### Recommended recovery classification

| Crash state                                   | Safe recovery                                                                                                              |
| --------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------- |
| `ModelRequestStarted` only                    | Close as abandoned/interrupted; never silently retry; allow next user turn                                                 |
| request with partial deltas but no completion | Preserve deltas in canonical trace; mark response incomplete/non-model-visible; allow a new turn with a visible diagnostic |
| `ToolRequested` only                          | Close as never executed and add protocol-completing tool result                                                            |
| `ToolStarted`, read-only                      | Reconcile/rerun under explicit recovery policy                                                                             |
| `ToolStarted`, mutating                       | Current reconciliation logic; block on uncertainty                                                                         |

This turns "fail closed" into **"fail closed only where state is genuinely ambiguous"**, which is a much better durability property for an agent harness.

---

# Lower-priority findings

I see three P2 issues worth cleaning up once the above are fixed.

First, `SessionSummary.closed` still checks whether **any** `SessionEnded` exists in the trace tail. A resumed session intentionally appends a new `SessionStarted { resumed: true }` after its previous `SessionEnded`, so an actively resumed session can still be reported as closed. The correct cheap calculation is based on the **latest session lifecycle event**: closed iff the most recent `SessionStarted`/`SessionEnded` is `SessionEnded`.

Second, checkpoint capsule publication renames the temporary capsule into place and calls `sync_data()` on the file, but does not appear to sync the containing directory. A power loss at the rename boundary can therefore leave a durable checkpoint projection referring to a missing capsule on filesystems where directory metadata was not persisted. Since the capsule is duplicated inside the semantic barrier, recovery could reconstruct the missing file instead of permanently refusing the session.

Third, the provider/MCP relay's proxy implementation now contains a sizable second network stack. In addition to DNS, plain-HTTP proxying is implemented through `CONNECT`, proxy precedence does not retain the target scheme when choosing environment variables, and the provider/MCP implementations duplicate substantial code. I classify these as P2 compatibility/maintenance concerns once the system-DNS P1 is removed.

---

# Round 4 verdict

The architecture is moving in a noticeably stronger direction. The **WAL + semantic projection validation + tool reconciliation** work in particular is a real improvement rather than a superficial patch.

I would nevertheless keep PR #93 open. The priority order I recommend is:

1. **Fix the Windows WAL failure and restore green Windows CI.**
2. **Replace the PID/directory lease with a crash-released OS lock.**
3. **Remove custom public-DNS fallback/system-resolver bypass from the HTTP cancellation design.**
4. **Implement explicit v1/v2 → v3 session migration before appending modern records.**
5. **Recover harmless interrupted request/tool lifecycle states instead of permanently bricking the session.**
6. Then clean up resumed-session closure metadata and checkpoint directory durability.

After those changes, **Round 5 should be narrower**. I would focus almost entirely on destructive failpoints—kill after every WAL/trace/session/checkpoint write—and cross-platform crash/reopen tests rather than another broad static pass. If those survive, I expect us to be getting fairly close to the point where I can say there are no remaining highly concerning issues.
