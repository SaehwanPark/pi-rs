# Tools & Sandbox Safety

`rupi` includes seven core built-in tools for agent coding tasks, designed around strict confinement, bounded output, and approval contracts.

---

## Built-In Tools

### 1. `read`
- **Purpose**: Inspect files within the workspace.
- **Parameters**: `path` (relative or absolute).
- **Behavior**: Verifies that the resolved path resides strictly within the workspace root (`--cwd`). If outside, the request is immediately denied with an out-of-bounds error.

### 2. `write`
- **Purpose**: Create or completely overwrite a file.
- **Parameters**: `path`, `content`.
- **Classification**: **Mutating**. Refused unless auto-approval is enabled.

### 3. `edit`
- **Purpose**: Perform structured search-and-replace edits on an existing file.
- **Parameters**: `path`, `old_text`, `new_text`.
- **Classification**: **Mutating**. Ensures atomic replacement without unexpected collateral drift.

### 4. `grep`
- **Purpose**: Search workspace text with bounded results.
- **Parameters**: Search pattern and path/options.
- **Classification**: **Read-only by default**; search-directory expansion is separately policy-controlled.

### 5. `exec`
- **Purpose**: Execute shell commands within the workspace directory.
- **Parameters**: `command`.
- **Classification**: **Mutating**.

`exec` uses the platform shell: `cmd.exe /C` on Windows and `sh -c` on Unix-like
systems. For a known executable, prefer `process` so each argument remains one
argv value and is not reinterpreted by shell quoting or expansion.

### 6. `process`
- **Purpose**: Execute a program directly with an explicit argument list.
- **Parameters**: `program`, optional `args`, `cwd`, and `timeout_ms`.
- **Classification**: **Mutating** — an arbitrary program may change workspace or external state.

---

## Safety Guarantees

### Workspace Confinement Root
At startup, the `--cwd` directory is canonicalized. File operations are checked with
realpath resolution so traversal and outward-pointing symlink components cannot escape
the workspace. Explicit policy flags are required to widen read, search, or write scope.

### Mutating Tool Guard
Mutating tools (`write`, `edit`, `exec`, `process`) cannot run automatically unless explicitly permitted in your configuration:

```json
{
  "tools": {
    "auto_approve_mutating": true
  }
}
```

If `auto_approve_mutating` is `false` (the default), mutating requests return an
explicit refusal event, preventing unintended side effects. Enable it only for a
trusted workspace.

### Process Boundaries
Neither tool is an OS-level sandbox. If running untrusted agent code, execute `rupi`
within Docker, a VM, or an isolated container sandbox.
