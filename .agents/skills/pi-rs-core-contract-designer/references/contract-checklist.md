# Core Contract Checklist

Use only the sections touched by the change.

## Boundary

- Which module owns the state?
- Is this fundamental runtime behavior or adapter/workflow policy?
- Can provider-, MCP-, Pi-, storage-, and TUI-specific details remain behind adapters?
- Does the design add work before the user can type or submit the first prompt?

## State and Identity

- Are states exhaustive and mutually understandable?
- Is `Unknown` preserved where completion cannot be proven?
- Do session, turn, event, tool-call, and model-epoch identities remain stable?
- Are parent/causal links explicit where replay needs them?
- Is ordering deterministic without relying on wall-clock uniqueness?

## Events

For every new event, record:

- why it exists
- what must happen before and after it
- whether it persists in session state, trace, both, or neither
- how replay interprets it
- whether the TUI should render it routinely, quietly, or prominently

## Provenance and Context

- Is native provider output distinct from provider summaries, declared rationales, and reconstruction?
- Is model-visible context clearly derived from canonical history?
- Can compaction remove prompt content without deleting trace evidence?
- Are external references attributable and rehydratable?

## Tools and Failover

- Does every tool call have durable identity and lifecycle?
- Are mutating and read-only/idempotent properties explicit?
- Can an uncertain mutation be reconciled without blind replay?
- Does retry happen before eligible failover?
- Are quality failures excluded from automatic failover?
- Does a backup capability check precede takeover?
- Does the backup continue from committed state rather than replay the whole turn?

## Persistence, Security, and Compatibility

- Is the schema versioned when durable?
- Is migration or unsupported old data handled explicitly?
- Does redaction occur before durable trace output?
- Are raw provider payloads still opt-in?
- Do project-local actions respect trust?
- Does the internal contract remain stronger than any single compatibility format?

## Test Matrix

At minimum, select applicable cases from:

- normal transition
- invalid transition
- duplicate input
- interruption before output
- interruption after partial stream
- failure after read-only tool
- failure after mutation
- unknown mutation completion
- serialization round trip
- older/newer schema behavior
- compaction with canonical trace preservation
- smaller backup context
- missing backup modality/tool capability
- redaction of sensitive payload
- deterministic event ordering
