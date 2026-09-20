# Fault-Tolerant Failover

In mission-critical agent workflows, primary model endpoints may experience outages, rate limits (HTTP 429), or transient network dropouts.

`rupi` implements resilient, capability-checked failover to an optional backup provider.

---

## Failover Policy: Recovery, Not Orchestration

In `rupi`:
- Exactly **one model is active** in normal execution.
- Backup model activation is strictly a **fault recovery** mechanism, never an autonomous orchestration pattern (such as routing different prompt categories to different models).
- Higher-level routing decisions belong outside the runtime core.

---

## Lazy Backup Instantiation

A configured backup adapter is constructed **only if a request actually requires it**.

If your primary model works flawlessly throughout the entire session, the backup provider costs zero CPU cycles, zero network checks, and zero startup overhead.

If a backup fails to initialize, it fails at the exact moment of failover, clearly naming the failing provider.

---

## Pre-Failover Capability Validation

Before switching traffic to a backup model, `rupi` compares the backup's declared capabilities against the current in-flight requirements:

1. **Tool Calling Support**: If the session is actively executing tool calls, a backup model lacking function-calling capabilities is **refused immediately by name** before sending any prompt.
2. **Context Window Reconciliation**: If the backup provider has a smaller context window than the primary, `rupi` attempts a safe context rebudget and records whether history was shortened. If the active turn or an uncertain tool boundary cannot be crossed safely, failover is refused explicitly instead of sending an oversized or replayed request.
3. **Modalities / Vision**: If the conversation relies on image inputs and the backup does not support vision, failover is rejected with a clear error rather than crashing the provider.
