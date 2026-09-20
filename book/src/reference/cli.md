# CLI Command Reference

Comprehensive reference of all CLI commands and options available in `rupi`.

---

## Global Synopsis

```text
Usage: rupi <command> [options]
```

Global Options:
- `-h, --help`: Display help information and exit.

---

## Commands

### `run`
Run one durable coding-agent turn in one shot.

```bash
rupi run --config <file> --cwd <workspace> --prompt <text> [options]
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
rupi interactive --config <file> [--cwd <workspace>]
```

---

### `trace`
Read a session's canonical event log out of the configured store in chronological order.

```bash
rupi trace --config <file> [session-id] [options]
```

The session id may be omitted to read the newest session. Additional selectors include
`--tools`, `--reasoning`, `--epoch <n>`, and `--sequence`; presentation flags include
`--color`, `--width`, `--no-reasoning`, `--verbose`, `--quiet`, and `--silent`.

---

### `replay`
Inspect a trace or session JSONL file without starting a provider or executing tools.

```bash
rupi replay <trace-or-session.jsonl> [options]
```

Use `--until`, `--tools`, `--reasoning`, `--timing`, `--context-at`, `--branch`,
`--compare`, `--json`, `--export`, and `--sequence` for deterministic analysis.

---

### `skills`
List skills discovered across user and project paths that would be presented to a model.

```bash
rupi skills [--project] [--trust-store <dir>]
```

---

### `prompts`
List prompt templates discovered across configured paths.

```bash
rupi prompts [--project] [--trust-store <dir>] [--prompt-template <path>] [--no-prompt-templates]
```

---

### `prompt`
Expand a single prompt template with positional parameters. This command only prints
text; it does not send a model request.

```bash
rupi prompt [--project] [--trust-store <dir>] <name> [arg1] [arg2] ...
```

---

### `packages`
List discovered Pi packages, inspect one package, or install an explicit local package.

```bash
rupi packages [--project] [--trust-store <dir>] [--show <name>]
rupi packages install [--project] <local-directory>
```

---

### `trust`
Record or inspect explicit project-level trust decisions. The store is supplied by the
caller and is never inferred from the project being trusted.

```bash
rupi trust --store <dir> --list
rupi trust --store <dir> --project <path> --grant
rupi trust --store <dir> --project <path> --deny
rupi trust --store <dir> --project <path> --clear
```

---

### `compat`
Audit an artifact or directory for Pi behavioral compatibility.

```bash
rupi compat <path>
```

---

### `import-pi` (legacy alias: `import`)
Plan or write an upstream Pi session JSONL file (or a directory of files) into the
native `rupi` store. Nothing is executed; use `--write` to persist the import.

```bash
rupi import-pi <session-path> [--config <file> | --store <dir>] [--write]
```

---

### `export`
Export a `rupi` session to an upstream Pi-compatible session JSONL format. Dropped
metadata is reported on stderr rather than silently relabelled.

```bash
rupi export <session-id> --config <file> [--out <file>]
```
