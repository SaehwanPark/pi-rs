# CLI Command Reference

Comprehensive reference of all CLI commands and options available in `pi-rs`.

---

## Global Synopsis

```text
Usage: pi-rs <command> [options]
```

Global Options:
- `-h, --help`: Display help information and exit.

---

## Commands

### `run`
Run one durable coding-agent turn in one shot.

```bash
pi-rs run --config <file> --cwd <workspace> --prompt <text> [options]
```

Flags:
- `--config <path>`: (Required) Path to JSON `RuntimeConfig` file.
- `--cwd <path>`: (Required) Confinement root directory.
- `--prompt <text>`: (Required) Input prompt text for the turn.
- `--resume <id|prefix>`: (Optional) Resume an existing stored session.
- `--trust-store <path>`: (Optional) Trust store directory path.

---

### `interactive`
Start an interactive terminal session holding context across multiple turns.

```bash
pi-rs interactive --config <file> [--cwd <workspace>]
```

---

### `trace`
Read a session's canonical event log out of the store in chronological order.

```bash
pi-rs trace <session-id>
```

---

### `replay`
Deterministically re-render a recorded session without executing tools or making network requests.

```bash
pi-rs replay <session-id>
```

---

### `skills`
List skills discovered across user and project paths that would be presented to a model.

```bash
pi-rs skills [--project] [--trust-store <dir>]
```

---

### `prompts`
List prompt templates discovered across configured paths.

```bash
pi-rs prompts [--project]
```

---

### `prompt`
Expand a single prompt template with positional parameters.

```bash
pi-rs prompt <name> [arg1] [arg2] ...
```

---

### `packages`
List discovered Pi packages and their contained surface definitions.

```bash
pi-rs packages
pi-rs packages install <local-directory>
```

---

### `trust`
Record or inspect explicit project-level trust decisions.

```bash
pi-rs trust list
pi-rs trust allow <path>
pi-rs trust deny <path>
```

---

### `compat`
Audit an artifact or directory for Pi behavioral compatibility.

```bash
pi-rs compat <path>
```

---

### `import`
Import an upstream Pi session JSONL file into the native `pi-rs` store, reporting any non-round-trippable attributes.

```bash
pi-rs import <session-path>
```

---

### `export`
Export a `pi-rs` session to an upstream Pi-compatible session JSONL format.

```bash
pi-rs export <session-id> --output <file>
```
