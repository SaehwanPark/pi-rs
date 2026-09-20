# Pi Ecosystem Compatibility

`rupi` is a clean Rust reimplementation with an explicit, tested subset of behavioral compatibility for the Pi ecosystem. Compatibility is strongest for skills, prompts, package discovery, selected extension APIs, and session migration; unsupported surfaces are reported rather than silently ignored.

---

## 1. Skills Discovery (`rupi skills`)

`rupi` reads the identical skill paths searched by upstream Pi:
- User skills: `~/.pi/agent/skills` and `~/.agents/skills`
- Project skills: `.pi/skills` and `.agents/skills` (searched upwards to the git repository root)

```bash
# List all user skills
rupi skills

# List user and project-local skills (explicit opt-in)
rupi skills --project
```

### Security Gate: Explicit Project Trust
Because skills inject instructions directly into the model's prompt, project-level skills are **never discovered silently**. They require the `--project` flag or explicit approval stored via `rupi trust`.

---

## 2. Prompt Templates (`rupi prompts` and `rupi prompt`)

List discovered prompt templates:
```bash
rupi prompts [--project]
```

Expand a template with positional parameters (`$1`, `$@`, `${1:-default}`, `${@:N:L}`):
```bash
rupi prompt review-pr "crates/rupi-core" "Focus on error handling"
```

---

## 3. Package Discovery & Installation (`rupi packages`)

Discover installed Pi packages:
```bash
rupi packages
```

Install a local package directory into the Pi package location:
```bash
rupi packages install ./my-pi-extension
```

---

## 4. Compatibility Diagnostics (`rupi compat`)

Audit any directory, package, or session for compatibility issues:
```bash
rupi compat ./my-pi-package
```

Reports supported features, unsupported runtime hooks, and migration advice.

---

## 5. Bidirectional Session Migration (`import` & `export`)

### Import Pi Sessions
Convert an upstream Pi session JSONL into the native `rupi` event store:
```bash
rupi import-pi ~/.pi/agent/sessions/2026-09-01-session.jsonl --config config.json --write
```
Non-round-trippable attributes (such as proprietary client metadata) are surfaced explicitly as warnings on `stderr`.

### Export to Pi Format
Export a `rupi` session back out to a standard Pi-compatible session file:
```bash
rupi export <session-id> --config config.json --out export.jsonl
```
