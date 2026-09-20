# Performance & Latency Budgets

`rupi` treats responsiveness as a user-facing contract. The repository keeps
benchmark scripts under `bench/`; they are release and pre-merge evidence, while
shared CI runs the startup benchmark on supported non-Windows runners.

## Budgets

| Metric | Budget | Evidence command |
| :--- | :---: | :--- |
| **Warm startup** (process invocation) | < 100 ms | `bash bench/startup.sh --json <path>` |
| **Cold startup** (fresh binary inode) | < 250 ms | `bash bench/startup.sh --cold --json <path>` |
| **Terminal keypress/render** | < 16 ms | `bash bench/keystroke.sh` and `bash bench/render.sh` |
| **Slash-command completion** | < 50 ms | Included in `bench/keystroke.sh` |
| **Large-session restore** | bounded by active projection | `bash bench/large_session.sh` |

Numbers vary by host, filesystem, terminal, and shared-runner load. A release note
should cite the command output and environment rather than presenting one machine's
latency as a universal guarantee. The cold-start script measures a fresh binary inode;
it does not drop the operating-system page cache and therefore is not a clean-boot
benchmark.

## Keeping optional work off the critical path

1. **No eager optional processes**: Node extension hosts, MCP servers, backup providers,
   and external indexes are initialized only at their explicit activation boundary.
2. **Bounded session hydration**: resume uses the latest checkpoint plus post-checkpoint
   projection instead of parsing the complete historical trace into model context.
3. **Append-only journals**: the semantic session log and canonical trace remain plain
   JSONL, while large redacted payloads use content-addressed blobs.
4. **Presentation/runtime separation**: rendering projects semantic events and does not
   perform network, filesystem, or tool mutations.

Performance changes to startup, rendering, resume, context, MCP, or extension loading
should include a benchmark result and explain any changed budget or measurement method.