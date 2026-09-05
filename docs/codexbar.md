---
title: "Identification of Used Percentages in AI subscription"
description: "How to identify usage and 5-hour and weekly usage limits via codexbar"
---

You are encouraged to periodically check current AI usage based on the current model provider to handle rate limiting and usage limits gracefully.

## Basic Policy

- For 5-hour limit, if the limit is approaching in less than 10 minutes, wait until the limit resets (use `sleep` until the estimated reset time + 30 seconds as buffer) before making new requests.
- For weekly limit, if the limit is approaching in less than 10 minutes, smartly determine where to stop (i.e., should be completed within 5 minutes). When the stop point is reached, open PR and merge into the main branch. Then stop.

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
