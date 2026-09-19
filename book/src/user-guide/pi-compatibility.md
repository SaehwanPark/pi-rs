# Pi Ecosystem Compatibility

`pi-rs` is a clean Rust reimplementation with an explicit, tested subset of behavioral compatibility for the Pi ecosystem. Compatibility is strongest for skills, prompts, package discovery, selected extension APIs, and session migration; unsupported surfaces are reported rather than silently ignored.

---

## 1. Skills Discovery (`pi-rs skills`)

`pi-rs` reads the identical skill paths searched by upstream Pi:
- User skills: `~/.pi/agent/skills` and `~/.agents/skills`
- Project skills: `.pi/skills` and `.agents/skills` (searched upwards to the git repository root)

```bash
# List all user skills
pi-rs skills

# List user and project-local skills (explicit opt-in)
pi-rs skills --project
```

### Security Gate: Explicit Project Trust
Because skills inject instructions directly into the model's prompt, project-level skills are **never discovered silently**. They require the `--project` flag or explicit approval stored via `pi-rs trust`.

---

## 2. Prompt Templates (`pi-rs prompts` and `pi-rs prompt`)

List discovered prompt templates:
```bash
pi-rs prompts [--project]
```

Expand a template with positional parameters (`$1`, `$@`, `${1:-default}`, `${@:N:L}`):
```bash
pi-rs prompt review-pr "crates/pi-rs-core" "Focus on error handling"
```

---

## 3. Package Discovery & Installation (`pi-rs packages`)

Discover installed Pi packages:
```bash
pi-rs packages
```

Install a local package directory into the Pi package location:
```bash
pi-rs packages install ./my-pi-extension
```

---

## 4. Compatibility Diagnostics (`pi-rs compat`)

Audit any directory, package, or session for compatibility issues:
```bash
pi-rs compat ./my-pi-package
```

Reports supported features, unsupported runtime hooks, and migration advice.

---

## 5. Bidirectional Session Migration (`import` & `export`)

### Import Pi Sessions
Convert an upstream Pi session JSONL into the native `pi-rs` event store:
```bash
pi-rs import-pi ~/.pi/agent/sessions/2026-09-01-session.jsonl --config config.json --write
```
Non-round-trippable attributes (such as proprietary client metadata) are surfaced explicitly as warnings on `stderr`.

### Export to Pi Format
Export a `pi-rs` session back out to a standard Pi-compatible session file:
```bash
pi-rs export <session-id> --config config.json --out export.jsonl
```
