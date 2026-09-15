# Reasoning Provenance

One of the central tenets of `pi-rs` is **honest provenance**.

Modern AI providers expose "reasoning" or "thinking" through very different mechanisms: some stream the model's authentic, unedited tokens; others stream a synthetic summary generated after the fact; and some conceal the chain-of-thought entirely.

Treating all of these as identical "thinking" misleads users and corrupts session context.

---

## The Four Provenance Labels

`pi-rs` strictly labels every chunk of reasoning text with its verified origin:

| Provenance Label | Meaning | When Used |
| :--- | :--- | :--- |
| `[native reasoning]` | Authentic model reasoning tokens | Provider streams true model tokens from an endpoint configured with `reasoning_kind: "native"` (e.g. DeepSeek-R1). |
| `[provider summary]` | Provider-synthesized explanation | Provider returns a summarized distillation of thinking while concealing the actual chain-of-thought (`reasoning_kind: "provider_summary"`). |
| `[declared]` | Declared rationale | Rationale explicitly declared by the model or tool caller within structured response fields. |
| `[reconstructed]` | Post-hoc reconstructed reasoning | Rationale inferred or reconstructed during import/export or session migration. Never conflated with live reasoning. |

---

## Architectural Invariants

1. **Declared Capability Wins**: The response field name alone cannot prove whether native tokens or a provider summary arrived. The endpoint declaration in the configuration decides the label. An endpoint declaring `provider_summary` is labeled `[provider summary]`, never `[native reasoning]`.
2. **Never Claim Hidden CoT Was Recovered**: When migrating sessions or importing from upstream Pi, hidden chain-of-thought is never fabricated or claimed to exist if it was omitted.
3. **No Provenance Bleeding**: Within a single reasoning turn, text fragments with different provenance boundaries are partitioned into distinct, unmerged spans.
