# Models & Providers

`pi-rs` connects to both local and remote AI models through a unified provider abstraction.

---

## Configuration Schema

The runtime configuration specifies primary and backup providers:

```json
{
  "provider": {
    "type": "openai",
    "base_url": "http://localhost:11434/v1",
    "api_key": "ollama",
    "model": "qwen2.5-coder:32b",
    "reasoning_kind": "native",
    "context_window": 32768,
    "max_tokens": 4096,
    "temperature": 0.2
  },
  "backup_provider": {
    "type": "openai",
    "base_url": "https://api.openai.com/v1",
    "api_key": "env:OPENAI_API_KEY",
    "model": "gpt-4o",
    "reasoning_kind": "none",
    "context_window": 128000
  },
  "state_dir": ".pi-rs-state",
  "tools": {
    "auto_approve_mutating": false
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
"reasoning_kind": "native"
```

### 2. vLLM / LM Studio / LocalAI
Any server exposing an OpenAI-compatible `/v1/chat/completions` endpoint can be plugged in directly by setting `base_url` and `model`.

### 3. OpenAI & Compatible Cloud Providers
- **OpenAI**: `https://api.openai.com/v1`
- **DeepSeek**: `https://api.deepseek.com/v1`
- **Together AI**: `https://api.together.xyz/v1`
- **Groq**: `https://api.groq.com/openai/v1`

Environment variables in `api_key` can be referenced with the `env:` prefix, e.g. `"api_key": "env:OPENAI_API_KEY"`, preventing credential leakage in config files.

---

## Declaring Reasoning Kind

Models expose thinking in fundamentally different ways. The provider configuration explicitly declares `reasoning_kind`:

- `"none"`: Standard completion models without thinking streams (e.g. GPT-4o).
- `"native"`: Raw model thinking tokens (e.g. DeepSeek-R1, Qwen reasoning models).
- `"provider_summary"`: Provider-generated summary of hidden reasoning (e.g. Claude thinking summaries).
- `"declared"`: Declared rationale provided alongside assistant completion.
