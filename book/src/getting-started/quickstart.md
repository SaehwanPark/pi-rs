# First session: local Qwen3.8-Flash-Next

This walkthrough uses the llama.cpp server already supported by rupi's OpenAI-compatible
provider. It is written for someone who has never used a coding harness.

## 1. Start llama.cpp

Keep this terminal open while rupi is running. In PowerShell, adapt the model path if your
GGUF files live somewhere else:

```powershell
llama serve `
  -m "C:\path\to\Qwen3.8-Flash-Next-UD-IQ4_XS-00001-of-00003.gguf" `
  --alias qwen3.8-flash-next `
  --host 127.0.0.1 `
  --port 8000 `
  --jinja `
  -ngl 999 `
  -c 262144 `
  -n 32768 `
  --cache-type-k q8_0 `
  --cache-type-v q8_0 `
  -b 2048 `
  -ub 512 `
  --flash-attn on `
  --reasoning on `
  --reasoning-effort xhigh `
  --reasoning-preserve `
  --temp 1.0 `
  --top-p 0.95 `
  --top-k 20 `
  --min-p 0.0 `
  --presence-penalty 0.0 `
  --repeat-penalty 1.0
```

The exact model file can be split across several GGUF files; pass the first file as shown
by your llama.cpp build. Confirm that the server is ready from a second terminal:

```powershell
(Invoke-RestMethod http://127.0.0.1:8000/v1/models).data
```

You should see the model id `qwen3.8-flash-next`. If you do not, copy the id that your
server reports into both `primary` and `model` below.

## 2. Create `config.json`

Create this file in the project you want rupi to inspect. It uses no cloud key and starts
with mutations refused:

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
        "context_window": 262144,
        "max_output_tokens": 32768
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

`primary` is the model rupi uses. `base_url` stops at `/v1`; rupi adds
`/chat/completions`. `exposed_reasoning: "native"` is a claim about what the endpoint
exposes, not a promise that every response contains reasoning text.

## 3. Run a read-only request

From the project directory:

```bash
rupi run --config config.json --cwd . \
  --prompt "List the top-level files and explain the project in three bullets. Do not change files."
```

PowerShell uses the same command on one line:

```powershell
rupi run --config config.json --cwd . --prompt "List the top-level files and explain the project in three bullets. Do not change files."
```

Two streams are intentional:

- **stdout** is the assistant's answer, so it can be piped into another command;
- **stderr** is the transcript: reasoning provenance, tool calls, warnings, and state changes.

A session directory appears under `.rupi-state/`. Do not commit that directory to your
project unless you deliberately want to share its redacted history.

## 4. Decide whether to allow changes

The default is safe: `write`, `edit`, and `exec` are refused. When you are in a workspace
you trust and want the model to edit it, change only this setting:

```json
"tools": {
  "auto_approve_mutating": true,
  "shell_timeout_ms": 120000,
  "max_output_bytes": 8192
}
```

Make a backup or use version control before enabling it. `--cwd` remains the confinement
root; rupi still refuses paths that escape it. Turn the setting off again when you return
to an unfamiliar repository.

## 5. Find and reuse a session

The run prints or records a session id. List the latest trace:

```bash
rupi trace --config config.json
```

Resume by id or a unique prefix:

```bash
rupi run --config config.json --cwd . --resume <session-id-or-prefix> \
  --prompt "Continue from the previous result; first summarize what is still unresolved."
```

Replay is read-only. It does not start llama.cpp, call a provider, or execute recorded
mutations:

```bash
rupi replay .rupi-state/sessions/<session-id>.trace.jsonl
```

For a live multi-turn editor, use:

```bash
rupi interactive --config config.json --cwd .
```

## Common fixes

| Symptom | What to check |
| :--- | :--- |
| `connection refused` | llama.cpp is still running and listening on `127.0.0.1:8000`. |
| `model not found` | `/v1/models` reports the exact id used in both config fields. |
| `invalid workspace` | `--cwd` exists and is the repository you intend to give the model. |
| No reasoning text | The server may expose only answer text; keep the provenance declaration honest. |
| A write is refused | Set `auto_approve_mutating` only in a trusted workspace. |
| Resume is refused | Use a complete session id or a prefix that matches exactly one session. |
| A Windows command fails | Use `process` with a direct argv list for known programs; use `dir`, not Unix `ls`, in `exec`. |
| A multi-file turn exhausts its budget | Split implementation and verification into separate bounded turns; exhaustion means incomplete work. |

If you use another OpenAI-compatible server, keep the same config shape and replace the
model id, URL, context window, and reasoning declaration with values that server documents.
