# Quickstart Guide

Get up and running with `pi-rs` in under 60 seconds.

---

## 1. Create a Configuration File

Create a minimal configuration file named `config.json` in your project or home directory. `pi-rs` uses standard JSON and supports any OpenAI-compatible endpoint (such as local Ollama, vLLM, LM Studio, or cloud providers).

### Example: Local Ollama (Qwen 2.5 Coder or Llama 3)

```json
{
  "provider": {
    "type": "openai",
    "base_url": "http://localhost:11434/v1",
    "api_key": "ollama",
    "model": "qwen2.5-coder:latest",
    "reasoning_kind": "native"
  },
  "state_dir": ".pi-rs-state",
  "tools": {
    "auto_approve_mutating": true
  }
}
```

### Example: Cloud Provider (OpenAI / DeepSeek)

```json
{
  "provider": {
    "type": "openai",
    "base_url": "https://api.openai.com/v1",
    "api_key": "env:OPENAI_API_KEY",
    "model": "gpt-4o",
    "reasoning_kind": "none"
  },
  "state_dir": ".pi-rs-state",
  "tools": {
    "auto_approve_mutating": false
  }
}
```

---

## 2. Run a One-Shot Task

Execute a single durable turn using `pi-rs run`:

```bash
pi-rs run \
  --config config.json \
  --cwd . \
  --prompt "Inspect Cargo.toml and list the workspace dependencies"
```

What happens:
- Output prose is streamed cleanly to `stdout`.
- Internal reasoning, tool invocation lifecycle, and diagnostics stream to `stderr`.
- An append-only session record is created in `.pi-rs-state`.

---

## 3. Launch an Interactive Session

For multi-turn terminal pair programming, start the interactive mode:

```bash
pi-rs interactive --config config.json
```

- Type your prompt in the multi-line editor buffer.
- Press `Enter` to submit.
- Press `Shift-Enter` or `Ctrl-J` to add a new line.
- Monitor model activity and token counts in the real-time statusline.
- Type `/help` or press `Ctrl-C` to interrupt an in-progress response.

---

## 4. Inspect or Replay Past Sessions

Every session is stored durably with typed events:

```bash
# List event trace for a session
pi-rs trace <session-id>

# Replay session execution deterministically
pi-rs replay <session-id>

# Resume where you left off
pi-rs run --config config.json --cwd . --resume <session-id> --prompt "Continue with next step"
```
