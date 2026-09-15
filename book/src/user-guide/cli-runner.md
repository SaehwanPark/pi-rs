# One-Shot CLI Runner

`pi-rs run` executes a single, complete, durable coding-agent turn against an explicitly configured endpoint. It is designed for headless workflows, CI/CD automation, scripts, and quick one-off coding tasks.

---

## Basic Invocation

```bash
pi-rs run --config <file> --cwd <workspace> --prompt <text>
```

![pi-rs CLI Runner](../assets/screenshots/cli-run.png)

---

### Options & Flags

| Flag | Required | Description |
| :--- | :---: | :--- |
| `--config <path>` | Yes | Path to JSON `RuntimeConfig` file. No hidden ambient config is loaded. |
| `--cwd <path>` | Yes | Confinement root for built-in tools. Canonicalized at startup. |
| `--prompt <text>` | Yes | Prompt instruction for the agent turn. |
| `--resume <id\|prefix>` | No | Resume an existing session by ID or prefix and append a new turn. |
| `--trust-store <path>` | No | Path to custom trust directory (consults `trust.json` before project discoveries). |
| `-h, --help` | No | Display CLI command help. |

---

## Stream Separation: stdout vs stderr

`pi-rs` strictly separates assistant prose from execution metadata:

- **`stdout`**: Streams only the raw, unattributed assistant response text. This makes `pi-rs run` directly pipeable into files or Unix pipelines (e.g. `pi-rs run ... > generated_code.rs`).
- **`stderr`**: Streams the structured runtime transcript, including:
  - Model request notifications
  - Provenance-labeled thinking text
  - Tool invocations, parameters, and return states
  - Diagnostic messages, warnings, and errors

---

## Security & Confinement Model

1. **Confinement Root**: All built-in file operations (`read`, `write`, `edit`) are strictly confined to `--cwd`. Attempts to access paths outside the workspace boundary (e.g., `../../etc/passwd`) are rejected by runtime contracts.
2. **Mutating Tool Guard**: Operations that alter disk state (`write`, `edit`, `exec`) are refused unless `"tools": { "auto_approve_mutating": true }` is present in `RuntimeConfig`.
3. **Execution Shell Escape**: The `exec` tool invokes the system shell. When enabled, it provides full system execution capability; use container or OS-level virtualization when isolating untrusted workloads.
