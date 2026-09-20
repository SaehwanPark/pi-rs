# Reasoning Provenance

One of the central tenets of `rupi` is **honest provenance**.

Modern AI providers expose "reasoning" or "thinking" through very different mechanisms: some stream the model's authentic, unedited tokens; others stream a synthetic summary generated after the fact; and some conceal the chain-of-thought entirely.

Treating all of these as identical "thinking" misleads users and corrupts session context.

---

## The Four Provenance Labels

`rupi` strictly labels every chunk of reasoning text with its verified origin:

| Provenance Label | Meaning | When Used |
| :--- | :--- | :--- |
| `[native reasoning]` | Authentic model reasoning tokens | Provider streams true model tokens from an endpoint configured with `capabilities.exposed_reasoning: "native"` (e.g. a local reasoning model). |
| `[provider summary]` | Provider-synthesized explanation | Provider returns a summarized distillation of thinking while concealing the actual chain-of-thought (`capabilities.exposed_reasoning: "provider_summary"`). |
| `[declared]` | Declared rationale | Rationale explicitly declared by the model or tool caller within structured response fields. |
| `[reconstructed]` | Post-hoc reconstructed reasoning | A reserved typed/display form for evidence-scoped analysis. The current runtime does not produce it automatically and never infers hidden reasoning during import/export. |

---

## Architectural Invariants

1. **Declared Capability Wins**: The response field name alone cannot prove whether native tokens or a provider summary arrived. The endpoint's `capabilities.exposed_reasoning` declaration decides the label. An endpoint declaring `provider_summary` is labeled `[provider summary]`, never `[native reasoning]`.
2. **Never Claim Hidden CoT Was Recovered**: When migrating sessions or importing from upstream Pi, hidden chain-of-thought is never fabricated or claimed to exist if it was omitted.
3. **No Provenance Bleeding**: Within a single reasoning turn, text fragments with different provenance boundaries are partitioned into distinct, unmerged spans.
