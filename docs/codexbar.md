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

- **Below 85%:** normal bounded work may continue.
- **85-94% (soft stop):** do not begin another substantive slice or spawn new model-backed workers. Finish the current bounded action, run necessary verification, push durable state, and prepare a concise resumable handoff.
- **95% or higher (hard stop):** finish only the immediate safe action, stop ongoing delegated work gracefully, persist current state, and generate a handoff. Do not start additional implementation or review turns.
- If usage rises by 20 or more percentage points between adjacent loop-boundary checks, treat the budget as rapidly draining: disable new delegation and finish the current slice even if usage remains below 85%.

Check at these points rather than running a dedicated monitor:

1. before starting a substantive development loop;
2. before starting the next slice;
3. after unusually model-heavy delegated/review work;
4. before intentionally launching parallel model-backed workers.

A usage check should not itself trigger more model-backed work. If the usage
provider is unavailable or ambiguous, report that uncertainty rather than
assuming there is sufficient headroom.

## Per-Provider Usage

#### Provider: OpenAI (ChatGPT subscription or openai-codex)

```bash
# Returns what is higher between 5-hour and weekly usage
codexbar --provider codex --json-only | jq '.[0].usage | [.primary.usedPercent, .secondary.usedPercent] | max'
```

```bash
# 5-hour only results
codexbar --provider codex --json-only | jq '.[0].usage.primary.usedPercent'

# When 5-hour limit is reset? (may be reset time point or remaining timedelta)
codexbar --provider codex --json-only | jq '.[0].usage.primary.resetDescription'

# Weekly only results
codexbar --provider codex --json-only | jq '.[0].usage.secondary.usedPercent'
```

For handoffs, record both percentages and the reset description so the next
session can distinguish a short-window stop from weekly exhaustion.

#### Provider:Cursor (whose models include Cursor models and 3rd-party models)

Cursor models (Grok, Composer):

```bash
# Monthly usage
codexbar --provider cursor --json-only | jq '.[0].usage.secondary.usedPercent'
```

Other models (3rd-party models):

```bash
# Monthly usage
codexbar --provider cursor --json-only | jq '.[0].usage.tertiary.usedPercent'
```

#### Provider: Google AI or AntiGravity (providing Gemini models)

Gemini models:

```bash
# Returns what is higher between 5-hour and weekly usage
codexbar --provider antigravity --json-only | jq '.[0].usage | [.primary.usedPercent, .extraRateWindows.[0].window.usedPercent] | max'
```

To know 5-hour usage:

```bash
# 5-hour usage amount
codexbar --provider antigravity --json-only | jq '.[0].usage.extraRateWindows.[0].window.usedPercent'

# When 5-hour limit is reset? (may be reset time point or remaining timedelta)
codexbar --provider antigravity --json-only | jq '.[0].usage.extraRateWindows.[0].window.resetDescription'
```

To know weekly usage:

```bash
codexbar --provider antigravity --json-only | jq '.[0].usage.primary.usedPercent'
```
