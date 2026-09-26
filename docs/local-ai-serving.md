Run llama.cpp to serve AI models locally. See the following CLI options. You may adjust options whenever justifiable.

## Linux

```bash
VK_DRIVER_FILES=/usr/share/vulkan/icd.d/radeon_icd.x86_64.json \
GGML_VK_VISIBLE_DEVICES=0 \
./build/bin/llama-server \
  -m ./UD-IQ4_XS/Qwen3.8-Flash-Next-UD-IQ4_XS-00001-of-00003.gguf \
  --alias qwen3.8-flash-next \
  --host 127.0.0.1 \
  --port 8000 \
  --jinja \
  -ngl 999 \
  -c 131072 \
  --cache-type-k q8_0 \
  --cache-type-v q8_0 \
  -b 2048 \
  -ub 512 \
  --flash-attn on \
  --reasoning on \
  --reasoning-effort medium \
  --reasoning-preserve \
  --temp 1.0 \
  --top-p 0.95 \
  --top-k 20 \
  --min-p 0.0 \
  --presence-penalty 0.0 \
  --repeat-penalty 1.0
```

## Windows 11

```powershell
llama serve `
   -m "C:\Users\saehwan\models\unsloth\Qwen3.8-Flash-Next-GGUF\UD-IQ4_XS\Qwen3.8-Flash-Next-UD-IQ4_XS-00001-of-00003.gguf" `
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
