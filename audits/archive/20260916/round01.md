## Round 1 audit

I audited the current `main` branch rather than relying on the repository's previous self-audits. The recent redaction work appears well targeted, and the latest recorded verification reports the full workspace passing 1,062 tests; I did not find reason to reopen those already-fixed redaction findings.

I do, however, find several **highly concerning issues**, so Round 1 is a clear **fix required**. I found no P0/critical defect, but I would treat the following five areas as P1/high priority.

### P1-1. Workspace write confinement can be escaped through symlinked path components

`Workspace::write_path()` performs only lexical normalization and then uses `starts_with(root)` for containment. It explicitly allows a symlink inside the workspace to point elsewhere.

The actual `write` and `edit` implementations then perform ordinary filesystem operations on that lexical path. Those operations follow symlinked parent directories.

For example:

```text
workspace/
  outside -> /home/user/important-dir
```

A write to:

```text
outside/config.json
```

passes the lexical workspace test as:

```text
/workspace/outside/config.json
```

but the filesystem operation reaches:

```text
/home/user/important-dir/config.json
```

This is especially concerning because the README currently advertises a **"Realpath-enforced root sandbox"**, while the implementation explicitly says it is not a sandbox and does not realpath model-supplied paths.

**Recommended fix:** make the file-operation boundary capability based, rather than validating a pathname and subsequently reopening it. `cap-std` would fit the project's Rust/no-unsafe style well: hold a workspace directory capability and perform opens, creates, and renames relative to it. A smaller interim fix would canonicalize the deepest existing ancestor and reject symlink traversal outside the root, but that retains TOCTOU exposure.

Regression tests should cover an outward-pointing parent symlink, an outward-pointing final symlink, `write` replace, `write --append`, and `edit`.

I would also remove the "realpath-enforced root sandbox" wording until that invariant is actually true.

---

### P1-2. Read-only tools can read outside the workspace by default

`Workspace::new()` defaults to:

```rust
allow_read_outside: true
```

while search and writes default to confined behavior.

The `read` tool consequently accepts an absolute path outside the project and is classified as read-only, so it bypasses the mutation approval gate. Its advertised schema nevertheless describes the path as "relative to the workspace."

More importantly, normal `ToolPolicy` configuration exposes no `allow_read_outside` setting. It exposes tool allow/deny, auto-approval, timeout, output limit, and cwd only.

For a cloud-backed coding agent, filesystem reading is not harmless: model-visible content is an egress path. A prompt-injected model that knows or guesses a path could request SSH keys, cloud credentials, shell-history files, files from an adjacent repository, etc., without obtaining the separately required `exec` approval.

**Recommended fix:** switch the default to `allow_read_outside = false`. Add explicit policy such as:

```text
filesystem:
  read_roots: [...]
  write_roots: [...]
```

or at minimum explicit `allow_read_outside`, `allow_search_outside`, and `allow_write_outside` controls. An outside-workspace read should either require an explicit configured root or pass through a human approval class for data exposure.

This is one of the issues I would fix first.

---

### P1-3. Cancellation is mostly classification, not interruption

The registry checks `CancelToken` before tool execution and checks it again afterward. It can therefore change the recorded terminal state to `Unknown`, but it cannot tell an already-running tool to stop.

The reason is architectural: `Tool::execute()` receives only the request and progress sink. It receives no `CancelToken` or execution context.

That makes the comment that tools should "not swallow cancellation" difficult to uphold: ordinary tools have no way to observe cancellation.

The extension host makes this substantially worse. A request holds the `HostInner` mutex and blocks in an unbounded `read_line()`. A hung JavaScript extension therefore prevents the caller from returning, while another call to `stop()` needs the same mutex.

The JS bootstrap itself simply `await`s extension handlers with no deadline.

Provider cancellation has a related latency problem: the OpenAI-compatible adapter checks cancellation between stream reads, but its default socket read timeout is **300 seconds**. A completely silent stalled stream can therefore remain inside the blocking read long after cancellation was requested.

**Recommended fix:** introduce a real execution context:

```rust
struct ToolExecutionContext {
  cancel: CancelToken,
  deadline: Deadline,
}
```

and pass it into every tool invocation.

For extension/MCP/provider boundaries, cancellation also needs an out-of-band way to interrupt blocking I/O. In particular:

* don't hold the only process-control mutex while blocking on protocol input;
* keep a killable process handle independently accessible;
* run protocol I/O through a worker with a bounded response channel and deadline;
* on cancellation/deadline, terminate the worker/process and record `Unknown` where side effects are possible;
* make provider reads cancellation-aware rather than relying on a five-minute read timeout.

A regression test should start a tool/extension that never returns, cancel the turn, and prove the runtime regains control promptly.

---

### P1-4. `exec` timeout does not guarantee that the command actually stops

The `exec` implementation launches a shell and, on timeout, calls:

```rust
child.kill()
```

It then waits for that direct child.

That does **not** generally kill subprocesses created by the shell. A command can spawn a child/background process that continues writing files, consuming GPU/CPU, or talking to the network after rupi reports that the timed-out command was killed.

This contradicts a particularly important safety comment in the implementation:

> a leaked child keeps running after we report a result

The current code prevents leaking the immediate shell, but not its process tree.

There is a second issue in the same path: stdout and stderr reader threads feed an ordinary `mpsc::channel`, which is unbounded. The code claims output is bounded "before it is buffered," but the capture limit is applied only after chunks leave that unbounded queue.  The configured capture ceiling itself is otherwise sensible.

**Recommended fix:** treat each exec as a process-tree unit.

On Unix, start the shell in its own process group/session and terminate the process group on timeout/cancellation. On Windows, use a Job Object or equivalent tree-termination mechanism. Replace the unbounded channel with a small `sync_channel` or equivalent bounded queue, and make sure reader workers terminate/join when the operation ends.

A strong test is:

1. execute a command that spawns a delayed child;
2. child would create a sentinel file after the timeout;
3. force timeout;
4. wait beyond the child's delay;
5. assert the sentinel file was never created.

That tests the actual guarantee rather than only checking that the shell PID disappeared.

---

### P1-5. Durable reconciliation of `write append=true` is unsound

`WriteTool` globally declares itself idempotent:

```rust
ToolMetadata::mutating("write", ..., true)
```

but the same tool supports:

```text
append=true
```

Appending is not idempotent.

More importantly, append reconciliation uses essentially:

```text
does the current file end with the requested appended bytes?
```

That does not establish that *this invocation* performed the append.

Consider:

```text
before call:     file already ends with "DONE\n"
requested append: "DONE\n"
```

If rupi durably records `ToolStarted` and crashes before `write()` executes, restoration sees the pre-existing suffix and can conclude that the call was committed even though nothing happened.

That undermines one of the project's strongest architectural contracts: `Unknown` side effects are supposed to be honestly reconciled rather than guessed. The core replay machinery explicitly relies on reliable reconciliation and idempotence metadata.

**Recommended fix:** the cleanest near-term solution is to remove append semantics from the idempotent `write` tool. Either:

* create a separate non-idempotent `append` tool whose uncertain outcome requires manual inspection, or
* make tool metadata request-dependent and durably record enough pre-operation state, such as file identity, size, and content digest, to prove that one append occurred.

Until such evidence exists, `reconcile()` for `append=true` should return `RequiresManualInspection`, never `Committed` merely from a suffix match.

A regression test should use the pre-existing-suffix/crash-before-mutation case above.

---

## P1-6. External line-oriented inputs are bounded too late

This one is slightly more systemic.

The SSE parser advertises a 1 MiB event limit, but it first executes:

```rust
reader.read_line(&mut line)
```

and checks the limit only afterward. An endpoint can send a giant line without a newline, causing `String` to grow far beyond the intended cap before the check runs.

The same pattern exists in:

* extension-host responses via `read_line`;
* MCP stdout and stderr via `BufRead::lines()`;
* local `read` and `grep`, where line truncation occurs only after `read_line` has allocated the complete line.

So several purported memory bounds are **output bounds rather than ingestion bounds**.

**Recommended fix:** implement one reusable bounded-line primitive that refuses a line once the byte ceiling is crossed without first materializing the whole thing. `BufRead::fill_buf`/`consume` or a carefully bounded `Take`/`read_until` implementation can do this.

Use separate sensible ceilings for provider events, extension JSON-RPC, MCP JSON-RPC, stderr diagnostics, and local text files. Protocol-boundary overflow should fail and close that transport rather than trying to discard an arbitrary unbounded remainder.

This deserves a common helper plus adversarial tests containing multi-megabyte newline-free inputs.

---

## Moderate issues worth fixing after the P1 set

Two additional items are not blockers by themselves but should enter the next implementation slice.

**Windows is not continuously tested.** The repository recently found and fixed multiple real Windows defects through local verification, including a P1 extension-host failure, yet CI currently runs only `ubuntu-latest` and `macos-latest`.   Given the amount of platform-specific process/path code, I would add `windows-latest` to the correctness matrix and leave Unix-specific startup benchmarking as a separate job if necessary.

**The README quickstart configuration appears stale relative to the current schema.** The example uses a top-level `"provider"` object, while `RuntimeConfig` currently requires fields such as `version`, `primary`, `thinking`, and uses `endpoints`.   This can make the 60-second onboarding path fail before users reach the runtime.

## Suggested remediation order

I would implement this as four tightly bounded slices rather than one large refactor:

1. **Filesystem safety:** P1-1 + P1-2.
2. **Cancellation/process supervision:** P1-3 + P1-4.
3. **Bounded ingestion:** P1-6 across provider/MCP/extensions/file tools.
4. **Durable tool semantics:** P1-5, preferably by separating append from replace.

Then add Windows CI and correct the README while the touched behavior is fresh.

### Round 1 verdict

**Not ready to conclude the audit loop.** I see **six P1/high-concern findings** and no P0 finding. The existing test suite and architecture are substantial, but several of these defects arise precisely where comments/tests currently claim stronger guarantees than the implementation provides.

Once the developers apply the fixes, send me back to the updated `main` or the remediation PR(s). For **Round 2**, I’ll first verify each of these six findings is genuinely closed with adversarial regression coverage, then audit another set of boundaries rather than simply rechecking the same code.
