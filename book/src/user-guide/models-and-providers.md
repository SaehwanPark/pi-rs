# Models & Providers

`pi-rs` connects to both local and remote AI models through a unified provider abstraction.

---

## Configuration Schema

The runtime configuration names a `primary` model and an optional `backup` model. Each
model must have a matching entry in `endpoints`:

```json
{
  "version": 1,
  "primary": "local/qwen2.5-coder:32b",
  "backup": "openai/gpt-4o",
  "thinking": "medium",
  "state_dir": ".pi-rs-state",
  "endpoints": [
    {
      "provider": "local",
      "model": "qwen2.5-coder:32b",
      "base_url": "http://localhost:11434/v1",
      "capabilities": {
        "text": true,
        "images": false,
        "tools": true,
        "exposed_reasoning": "native",
        "context_window": 32768
      }
    },
    {
      "provider": "openai",
      "model": "gpt-4o",
      "base_url": "https://api.openai.com/v1",
      "api_key_env": "OPENAI_API_KEY",
      "capabilities": {
        "text": true,
        "images": true,
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

## Supported Endpoints

### 1. Ollama (Local)
Run models locally with zero external network dependencies:
```bash
ollama run qwen2.5-coder:latest
```
Config snippet:
```json
"base_url": "http://localhost:11434/v1",
"model": "qwen2.5-coder:latest",
"capabilities": { "exposed_reasoning": "native", "context_window": 32768 }
```

### 2. vLLM / LM Studio / LocalAI
Any server exposing an OpenAI-compatible `/v1/chat/completions` endpoint can be plugged in directly by setting `base_url` and `model`.

### 3. OpenAI & Compatible Cloud Providers
- **OpenAI**: `https://api.openai.com/v1`
- **DeepSeek**: `https://api.deepseek.com/v1`
- **Together AI**: `https://api.together.xyz/v1`
- **Groq**: `https://api.groq.com/openai/v1`

Reference credentials through `api_key_env` (for example, `"api_key_env": "OPENAI_API_KEY"`) rather than placing a secret in the configuration. Literal keys are accepted for local endpoints but are never serialized back out.

---

## Declaring Reasoning Kind

Models expose thinking in fundamentally different ways. The endpoint's
`capabilities.exposed_reasoning` declaration determines the provenance label:

- `"none"`: No declared reasoning stream.
- `"native"`: Native model reasoning exposed by the endpoint.
- `"provider_summary"`: Provider-authored summary of hidden reasoning.
- `"declared"`: Rationale intentionally requested or declared alongside the completion.

The top-level `thinking` field controls the requested depth (`off`, `minimal`, `low`,
`medium`, `high`, or `xhigh`); it does not change the provenance claim.
