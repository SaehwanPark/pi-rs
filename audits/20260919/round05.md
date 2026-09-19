# Round 5 audit

I audited current `main` at **`fb80c7c8833fe793602030e26514207191350d74`**, including merged PR #95. The PR head's CI is fully green on **Ubuntu, macOS, and Windows**: fmt, clippy, tests, docs, and mdBook all pass.

The Round 4 remediation is materially good. In particular, Windows WAL reuse now works, leases use OS-backed file locks, custom public DNS resolution is gone, legacy session logs migrate before modern appends, safe interrupted model/tool lifecycles are normalized, resumed-session closure status is corrected, and checkpoint publication is more durable.

The narrower failpoint pass still finds several issues I would fix before calling the durability architecture complete.

**No P0. Five P1/high-concern findings remain.**

## Round 4 disposition

| Round 4 finding                                           | Round 5 disposition                                                                                                                                                                                  |
| --------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Windows WAL `Access is denied`                            | **Closed.** `LineWriter::clear()` now closes append handle, truncates through a dedicated handle, syncs, then reopens append; Windows CI passes.                                                     |
| Stale leases after crash                                  | **Mostly closed.** Kernel-backed locking is the right design and kill/reacquire is tested cross-platform. A narrower crash-poison window remains below.                                              |
| Cancellation relay bypasses system DNS                    | **Closed as originally reported.** Resolution now uses the platform resolver rather than a custom DNS client/public fallback.                                                                        |
| v1 sessions can become self-incompatible                  | **Closed.** Migration occurs while holding the lease before modern append handles are opened.                                                                                                        |
| Ordinary crashes during model/tool lifecycle brick resume | **Substantially closed.** Open model requests become explicitly `abandoned`; pre-`ToolStarted` requests become failed/no-side-effect results. Started mutating calls still reconcile conservatively. |

The two Round 4 P2s I specifically called out are also improved: session `closed` now follows the latest lifecycle event, and checkpoint files are synced before publication with directory sync on Unix.

---

# P1-1 — Torn JSONL tails are still incompatible with the recovery architecture

This is now the largest durability gap I see.

The JSONL layer itself correctly acknowledges that a process can die while writing the final line. `last_valid_seq()` even explicitly skips a torn final line to recover the preceding canonical sequence.

But the store above it effectively says:

```text
any malformed trace line → refuse resume
any malformed session line → refuse resume
any malformed WAL line → refuse WAL recovery
```

For example, `Store::resume()` checks the session report and journal's `malformed` count **before** projection recovery. `ProjectionWal::read_pending()` similarly rejects a WAL containing any malformed line.

That defeats the WAL exactly at an important failpoint:

```text
WAL prepare starts writing
           ↓ kill here
partial final WAL line

or

canonical trace event starts writing
           ↓ kill here
partial final trace line

or

semantic projection starts writing
           ↓ kill here
partial final session line
```

These are not necessarily corruption. A torn **final** line is the ordinary representation of "the process stopped during this append."

There is also a subtler second case. The bounded reader accepts a final JSON value that is syntactically complete even when it has **no terminating newline**. `LineWriter::create()` then opens the file for append without normalizing the tail. The next write can produce:

```text
{"old":"valid"}{"new":"valid"}
```

turning a recoverable tail into permanent JSONL corruption.

### Why this is P1

This is exactly the failure mode the WAL is meant to handle. A SIGKILL at the wrong filesystem-write boundary can still make a session permanently non-resumable before the recovery state machine gets a chance to act.

### Recommended fix

Add an explicit **append-tail recovery primitive** used before opening any trace/session/WAL writer while the session lease is held.

For the final physical line only:

```text
file ends in newline
    → normal validation

file has unterminated tail, tail parses as valid record
    → append newline + sync
    → treat record as present

file has unterminated tail, tail is invalid
    → truncate back to previous newline + sync
    → treat attempted append as not committed
```

Interior malformed lines must continue to fail closed.

Then let the WAL decide whether the discarded/preserved final transaction needs projection repair.

I would add forced fixtures for every durable file with both:

* half-written final JSON;
* complete JSON with the final newline missing.

This is an important prerequisite to meaningful kill-at-every-write testing.

---

# P1-2 — Blob references can become durable before the blob itself is durable

`BlobStore::put()` uses the correct overall pattern—temporary file, verification, rename—but the durability ordering is weaker than the journal/checkpoint paths.

Today it effectively does:

```rust
file.write_all(&encoded)?;
file.flush()?;
verify_file(&temp, &blob)?;
fs::rename(&temp, &final_path)?;
```

There is no `sync_all()`/`sync_data()` on the temporary blob before rename, and no containing-directory sync afterward.

Immediately afterward, the caller can synchronously persist and fsync a canonical event containing the returned `BlobRef`.

That gives a power-loss ordering like:

```text
write blob into page cache
rename blob
          ↓
fsync trace/session event referring to blob
          ↓
power loss
```

After restart it is possible for the **reference to survive while the blob or rename does not**. `Store::restore()` correctly verifies referenced blobs and then refuses continuation, but by then a recoverable agent session has become unusable.

This affects more than optional telemetry. Blobs can carry:

* reduced context recovery data;
* externalized canonical event fields;
* tool-result recovery content;
* compaction summaries;
* other model-visible payloads.

### Recommended fix

Use the same publication discipline as checkpoint capsules:

```text
create unique temporary
→ write
→ sync_all(temp)
→ verify temp
→ rename to content-addressed final
→ sync containing directory where supported
→ only then return BlobRef
```

On rename races, do not merely treat `final_path.exists()` as success. **Verify the winner's content** before returning the reference.

There is also a related P2: the supposedly "per-write" temporary suffix is actually just `part-{pid}`. Concurrent writes from two threads in the same process can therefore share the same temp path. Use a UUID/counter or `create_new`.

---

# P1-3 — Automatic compaction/checkpoint transactions still have crash states that permanently refuse resume

The WAL has greatly improved atomicity for single event→projection transactions. The remaining problem is the **multi-event** context lifecycle.

Ordinary L1/L2 compaction does roughly:

```text
ContextCompactionStarted
ContextSummary + semantic summary
ContextCompactionEpoch
ContextCompactionCompleted
only then mutate live message vector
```

Importantly, the runtime intentionally postpones the in-memory history replacement until all durable writes succeed. Therefore, if it dies before `ContextCompactionCompleted`, the correct pre-crash state is actually knowable:

> the old model-visible history remains authoritative; the incomplete attempted compaction can be aborted.

But the resume path currently encounters the outstanding compaction-start WAL intent and does:

```text
incomplete context-compaction lifecycle → refuse resume
```

So an automatic context-management operation can permanently brick an otherwise unambiguous session.

Checkpointing has the analogous boundary:

```text
checkpoint capsule
CheckpointCreated + barrier
          ↓ kill here
ContextCompactionCompleted(L3)
```

`validate_checkpoint_lifecycles()` rejects a modern checkpoint with no completion. Yet once `CheckpointCreated` and its semantic barrier are durable, the checkpoint record already contains enough information—context epoch, summarized count, capsule—to deterministically finish or normalize that boundary before any later provider request.

### Recommended fix

Make multi-event context transformations recoverable transactions rather than merely detectable incomplete lifecycles.

For L1/L2, the simplest semantics are probably:

```text
no completion → append durable CompactionAborted/rollback marker
              → ignore staged summary in resumed model projection
              → retain old context
```

Or stage the summary projection itself until the compaction commits.

For L3:

```text
durable CheckpointCreated + matching barrier
+ missing L3 completion
→ synthesize/append the deterministic completion during recovery
```

Anything structurally contradictory should still fail closed.

Tests should kill after **every** individual compaction/checkpoint durable append and prove that restart results in either the exact old context or the exact committed new context—never permanent refusal merely because the process stopped between two internal bookkeeping writes.

---

# P1-4 — The new kernel lease still has a narrow crash-poison state

Moving ownership to an OS-backed file lock was the correct fix. The normal kill test now passes.

The remaining issue is that every acquisition rewrites the persistent lock marker like this:

```rust
file.set_len(0)?;
file.seek(...)?;
writeln!(file, "pi-rs-lock-v1 ...")?;
file.sync_all()?;
```

Consider a session whose lease directory already has an `owner` file:

```text
acquire kernel lock
set_len(0)
        ↓ kill / disk error here
```

The kernel lock is correctly released on death, but the on-disk `lock` file is now empty while the old `owner` marker remains.

On the next acquisition, an empty lock file plus owner marker is interpreted as the **legacy pre-lock format**.

This is particularly problematic on Windows because:

```rust
#[cfg(windows)]
fn legacy_process_alive(_pid: u32) -> bool {
  true
}
```

so that session can become permanently "active elsewhere" despite having no kernel lock at all.

There is a variant on Unix too: the stale `owner` marker may identify an older process that is still alive even though it no longer owns this session, causing a false legacy-live result.

The same compatibility logic means genuinely old-format Windows leases from the brief pre-kernel-lock implementation cannot ever be automatically reclaimed.

### Recommended fix

Make the **presence of the lock file format durable and immutable**.

For example:

```text
if lock file does not exist:
    inspect legacy owner
    safely migrate old lease
    create_new lock file
    write format marker
    sync

thereafter:
    never truncate/rewrite the lock-format file
    simply open + kernel-lock it
```

Put PID/token diagnostics in the separate `owner` file only.

That removes the ambiguous "zero-length new lock looks like legacy lock" state entirely.

For actual legacy Windows owner markers, either use a real Windows process-liveness API during one-time migration or expose an explicit safe migration/reclaim path.

Add a failpoint specifically between `set_len(0)` and marker publication—or preferably eliminate that destructive rewrite so such a failpoint no longer exists.

---

# P1-5 — Literal model API keys are still serializable and printable through public config types

This is unrelated to Round 4 but worth fixing before declaring the security boundaries settled.

`ModelEndpoint` says:

> Literal credential ... Never serialized back out.

But it is declared as:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelEndpoint {
    ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}
```

`skip_serializing_if` only omits `None`.

So:

```rust
let endpoint = ModelEndpoint {
    api_key: Some("sk-secret".into()),
    ...
};

serde_json::to_string(&endpoint)
```

**contains the key.**

`RuntimeConfig` also derives ordinary `Debug`, so formatting a parsed runtime config exposes literal endpoint keys as well.

The custom `RuntimeConfig::to_json_string()` manually removes `api_key`, which is good for that one code path, but the public types still violate their documented secret contract.

`ProviderConfig` is somewhat better on serialization (`#[serde(skip_serializing)]`), but it too derives `Debug`, so directly debugging a `ProviderConfig` prints its literal credential and potentially sensitive header values. `OpenAiCompat`'s custom `Debug` only protects debug-printing the adapter.

### Recommended fix

Make this structural rather than relying on selected call sites:

```rust
#[serde(default, skip_serializing)]
api_key: Option<String>
```

and provide custom redacted `Debug` implementations—or ideally a dedicated secret wrapper whose `Debug`/`Display` are always redacted.

Add tests against the underlying traits, not only `RuntimeConfig::to_json_string()`:

```text
serde_json::to_string(ModelEndpoint) does not contain key
format!("{ModelEndpoint:?}") does not contain key
format!("{RuntimeConfig:?}") does not contain key
format!("{ProviderConfig:?}") does not contain key
```

I would also make provider URL validation reject userinfo credentials, mirroring the MCP URL rule.

---

## P2 findings

The new platform DNS approach fixes the privacy problem, but `resolve_target()` now spawns a fresh thread around `ToSocketAddrs`. Cancellation/deadline returns without joining that DNS thread because the platform resolver call itself is not cancellable. Repeated timeouts against a wedged resolver can therefore accumulate detached resolver threads. I would use a bounded resolver worker/pool or move to a networking stack whose resolver/request future is cancellable. This is **P2** because ordinary OS DNS eventually times out, but it contradicts the otherwise strong "joined shutdown" lifecycle.

Schema migration re-applies the **current** redaction policy to semantic checkpoint barriers but does not rewrite their external checkpoint capsule files. If the redaction policy changed since the old session was written, the migrated inline capsule can disagree with the old file and later checkpoint verification will reject resume. Migration should either leave historical payload bytes untouched or migrate all linked durable representations together.

The blob temporary name is process-scoped rather than write-scoped as mentioned above, and uncommitted/orphan checkpoint/blob/temp artifacts are not aggressively reclaimed after recovery. These are bounded today, so I would keep them below P1.

Finally, the WAL is now doing several synchronous writes and syncs around many semantic events. Correctness comes first, but once the remaining crash semantics are finished I would benchmark **turn/tool latency**, not only process startup. The present startup benchmark would not detect durability-induced interactive latency.

# Round 5 verdict

The gap from Round 4 to Round 5 is smaller than the earlier rounds. I no longer see broad architectural safety problems in ordinary execution. The remaining high-concern work is concentrated in **true crash/power-loss durability and one credential boundary**.

My fix order would be:

1. **Make all three JSONL journals append-safe after torn tails.**
2. **Make BlobRef publication genuinely durable before a durable reference can exist.**
3. **Give incomplete L1/L2/L3 context transactions deterministic recovery.**
4. **Eliminate destructive rewriting of the kernel-lock format marker.**
5. **Make literal credentials non-serializable/non-debuggable by construction.**

After those, I would make **Round 6 almost entirely test-driven**: add a deterministic failpoint harness around durable filesystem operations and enumerate `kill after write N` across a representative turn with model streaming, tool execution, L0/L1/L3 reduction, and resume. If that matrix passes on Linux/macOS/Windows, I expect the bar for “no remaining highly concerning issues” to be quite close.
