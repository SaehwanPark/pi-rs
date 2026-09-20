# rupi

<p align="center">
  <strong>A small, observable coding harness for local and remote models.</strong>
</p>

<p align="center">
  <a href="https://github.com/SaehwanPark/rupi/actions/workflows/ci.yml"><img src="https://github.com/SaehwanPark/rupi/actions/workflows/ci.yml/badge.svg" alt="CI Status" /></a>
  <a href="https://saehwanpark.github.io/rupi/"><img src="https://img.shields.io/badge/docs-GitHub%20Pages-blue.svg" alt="Documentation" /></a>
  <a href="https://github.com/SaehwanPark/rupi/releases"><img src="https://img.shields.io/badge/release-v0.2.1-green.svg" alt="Release v0.2.1" /></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/rust-1.85%2B%20(2024)-orange.svg" alt="Rust 1.85+" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-purple.svg" alt="License" /></a>
</p>

---

> **Minimal core. Observable execution. Honest provenance. Recoverable state.**

**rupi is not a Pi rewrite or Rust port.** It is an independent coding harness that
keeps a small terminal workflow while offering behavioral compatibility with selected Pi
skills, prompts, packages, extensions, and session files.

The current release is **v0.2.1**. The easiest first model is a local
[llama.cpp](https://github.com/ggml-org/llama.cpp) server running
Qwen3.8-Flash-Next; cloud OpenAI-compatible endpoints work too.

## What rupi does

- **Runs in a terminal:** use `rupi run` for scripts or `rupi interactive` for a
  multi-turn session.
- **Uses your model:** connect to a local or remote OpenAI-compatible endpoint; no
  provider is contacted until you submit a turn.
- **Keeps you in control:** reads stay inside `--cwd`; writing and shell tools are
  disabled unless you explicitly enable them for a trusted workspace.
- **Preserves evidence:** sessions, tool outcomes, exposed reasoning provenance, and
  failover boundaries are recorded in `.rupi-state`.
- **Stays inspectable:** `rupi trace` reads a session and `rupi replay` examines history
  without contacting a model or re-running tools.

## Quick start

### 1. Install rupi

From a local checkout:

```bash
git clone https://github.com/SaehwanPark/rupi.git
cd rupi
cargo install --path .
```

Or download a matching binary from the
[GitHub Releases](https://github.com/SaehwanPark/rupi/releases) page.

### 2. Start a local model (optional)

The beginner guide contains the complete PowerShell command for the supplied
Qwen3.8-Flash-Next llama.cpp setup. The important endpoint is:

```text
http://127.0.0.1:8000/v1
model: qwen3.8-flash-next
```

### 3. Create `config.json`

```json
{
  "version": 1,
  "primary": "local/qwen3.8-flash-next",
  "thinking": "xhigh",
  "state_dir": ".rupi-state",
  "endpoints": [
    {
      "provider": "local",
      "model": "qwen3.8-flash-next",
      "base_url": "http://127.0.0.1:8000/v1",
      "capabilities": {
        "text": true,
        "images": false,
        "tools": true,
        "exposed_reasoning": "native",
        "context_window": 262144
      }
    }
  ],
  "tools": {
    "auto_approve_mutating": false,
    "shell_timeout_ms": 120000,
    "max_output_bytes": 8192
  }
}
```

### 4. Ask a read-only question

```bash
rupi run --config config.json --cwd . \
  --prompt "List the top-level files and explain what this project is. Do not change files."
```

Assistant text goes to `stdout`; the labelled transcript and diagnostics go to `stderr`.
By default rupi refuses `write`, `edit`, and `exec`. Only set
`auto_approve_mutating` to `true` in a workspace you trust.

### 5. Continue or inspect the session

```bash
rupi interactive --config config.json --cwd .
rupi trace --config config.json <session-id>
rupi replay .rupi-state/sessions/<session-id>.trace.jsonl
```

If the model server is unavailable, check that it is listening on port 8000 and that the
configured model name exactly matches its `/v1/models` response. See the
[beginner guide](https://saehwanpark.github.io/rupi/getting-started/quickstart.html) for
step-by-step recovery help.

## Commands

| Command | Purpose |
| :--- | :--- |
| `rupi run` | One durable, scriptable turn |
| `rupi interactive` | Multi-turn terminal session |
| `rupi trace` | Read the canonical event transcript |
| `rupi replay` | Inspect history without generation or tool execution |
| `rupi skills`, `prompts`, `packages` | Discover Pi-compatible artifacts |
| `rupi compat` | Report compatibility by surface |
| `rupi import-pi`, `export` | Move sessions to and from Pi JSONL |
| `rupi trust` | Record explicit project trust decisions |

Run `rupi <command> --help` for flags and examples.

## Screenshots

The public guide includes screenshots built from transcript and output captured from the real
local Qwen3.8-Flash-Next endpoint:

![rupi interactive terminal](assets/screenshots/interactive-tui.png)

![rupi one-shot runner](assets/screenshots/cli-run.png)

![rupi trace and replay](assets/screenshots/trace-replay.png)

## Documentation and design

- **[Beginner guide](https://saehwanpark.github.io/rupi/)** — install, connect a model,
  make a safe first request, and recover from common setup errors.
- **[Architecture](ARCHITECTURE.md)** — runtime boundaries and invariants.
- **[Compatibility](COMPATIBILITY.md)** — measured upstream Pi behavior.
- **[Contributing](CONTRIBUTING.md)** — focused changes and verification commands.
- **[Roadmap](ROADMAP.md)** — current project state.

## License

`rupi` is distributed under the [MIT License](LICENSE).
