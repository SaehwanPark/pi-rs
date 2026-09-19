---
title: "Identification of Used Percentages in AI subscription"
description: "How to identify usage and 5-hour and weekly usage limits via codexbar"
---

Check current AI usage at bounded workflow points so long-running agentic work can
stop gracefully before exhausting a subscription window. Usage checks are cheap
local observations and must be run by the current/parent agent; never spawn a
model-backed subagent solely to monitor usage.

## Basic Policy

Use the highest relevant used percentage reported for the active provider.

- **Below 98%:** normal bounded work may continue.
- **98% or higher:** if 5-hour usage percentage is higher, check when their limit is reset, then wait until the 5-hour limit is reset (use `sleep <seconds>`). Wait two more minutes before resuming work. The same rule applied to weekly usage/limit. You should **not** check the usage/limit when you are waiting (to save tokens).

Check at these points rather than running a dedicated monitor:

1. before starting a substantive development loop;
2. before starting the next slice;
3. after unusually model-heavy delegated/review work;
4. before intentionally launching parallel model-backed workers.

If the usage provider is unavailable or ambiguous, assume there is sufficient headroom.

## Tool to use

Use [ai-usage-monitor](https://github.com/SaehwanPark/ai-usage-monitor).

```bash
uvx --from ../ai-usage-monitor usage <subscription>
```

Replace `<subscription>` with the desired subscription name among `codex`, `cursor`, `antigravity`.

- `codex`: OpenAI Codex subscription. Use this when you use OpenAI GPT models.
- `cursor`: Cursor subscription. Use this when you use models via Cursor routing (both Cursor-provided models and 3rd-party models). Note this subscription does have only monthly usage limits.
- `antigravity`: Google AI subscription. Use this when you use Gemini models via Antigravity.
