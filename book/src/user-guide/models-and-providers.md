# Models and providers

rupi talks to one model at a time through an OpenAI-compatible HTTP endpoint. The endpoint
can be local (the supplied llama.cpp server or another compatible implementation) or remote
(OpenAI and compatible services). The model is configured explicitly; rupi does not discover
or contact providers at startup.

## Local llama.cpp (recommended first setup)

The supplied Qwen3.8-Flash-Next setup listens at:

```text
http://127.0.0.1:8000/v1
model: qwen3.8-flash-next
```

Start it with the command in the [quickstart](../getting-started/quickstart.md#1-start-llamacpp),
then use this endpoint entry:

```json
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
```

`thinking: "xhigh"` asks rupi for the highest supported reasoning setting. llama.cpp's
`--reasoning on`, `--reasoning-preserve`, and chat template determine whether reasoning
arrives as a separate stream. The capability declaration must describe the endpoint you
actually run.

Check the server before debugging rupi:

```powershell
(Invoke-RestMethod http://127.0.0.1:8000/v1/models).data
```

## Other OpenAI-compatible servers

Any server exposing `/v1/chat/completions` can be used. Change `base_url`, `model`, and
capabilities together, then verify the server's `/v1/models` response before running a turn.
The Qwen3.8-Flash-Next setup above remains the supported beginner path for this release.

## Remote OpenAI-compatible endpoints

A remote endpoint should name its key through an environment variable, never in a checked-in
config file:

```json
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
```

Set the variable in the shell that launches rupi. The runtime redacts credentials when it
serializes configuration and trace data.

## Reasoning provenance

The endpoint's declared `exposed_reasoning` value controls the label attached to a reasoning
stream:

- `none`: no reasoning stream is claimed;
- `native`: the endpoint declares model-emitted reasoning is exposed;
- `provider_summary`: the provider supplies a transformed summary;
- `declared`: the text is an intentional rationale, not private chain-of-thought.

A field named `reasoning_content` is not enough to prove native reasoning. See
[Reasoning provenance](reasoning-provenance.md) for the full rule.

## Backup models

A backup is for availability recovery, not for voting or quality routing. Add a `backup`
model reference and a matching endpoint only when you want rupi to continue after a
retryable provider failure. The backup is initialized lazily and capability-checked before
it receives the session.
