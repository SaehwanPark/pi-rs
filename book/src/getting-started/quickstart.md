# Quickstart Guide

Get up and running with `pi-rs` in under 60 seconds.

---

## 1. Create a Configuration File

Create a minimal configuration file named `config.json` in your project or home directory. `pi-rs` uses a small versioned JSON schema and supports any OpenAI-compatible endpoint (such as local Ollama, vLLM, LM Studio, or cloud providers). The `primary` and `endpoints` entries use the same `provider/model` identity.

### Example: Local Ollama (Qwen 2.5 Coder or Llama 3)

```json
{
  "version": 1,
  "primary": "local/qwen2.5-coder:latest",
  "thinking": "medium",
  "state_dir": ".pi-rs-state",
  "endpoints": [
    {
      "provider": "local",
      "model": "qwen2.5-coder:latest",
      "base_url": "http://localhost:11434/v1",
      "capabilities": {
        "text": true,
        "images": false,
        "tools": true,
        "exposed_reasoning": "native",
        "context_window": 32768
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

### Example: Cloud Provider (OpenAI / DeepSeek)

```json
{
  "version": 1,
  "primary": "openai/gpt-4o",
  "thinking": "off",
  "state_dir": ".pi-rs-state",
  "endpoints": [
    {
      "provider": "openai",
      "model": "gpt-4o",
      "base_url": "https://api.openai.com/v1",
      "api_key_env": "OPENAI_API_KEY",
      "capabilities": {
        "text": true,
        "images": false,
        "tools": true,
        "exposed_reasoning": "none",
        "context_window": 128000
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
- Mutating tools remain refused unless you explicitly set `auto_approve_mutating` to `true` for a trusted workspace.
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
# Read the event trace for a session
pi-rs trace --config config.json <session-id>

# Inspect a recorded trace or session file deterministically
pi-rs replay path/to/session.trace.jsonl

# Resume where you left off
pi-rs run --config config.json --cwd . --resume <session-id> --prompt "Continue with next step"
```
