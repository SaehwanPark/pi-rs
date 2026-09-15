## Subagent Delegation and Context Management

Stay single-agent by default. Subagents are context-isolation and specialization
tools, not the normal execution path. Delegate only when the expected context,
quality, or latency benefit clearly outweighs the extra model-turn cost.

### Budget and routing guardrails

* Default to one active model-backed child at a time.
* Use two concurrent children only for clearly independent tasks when parallelism has a concrete benefit and subscription usage has healthy headroom.
* Do not recursively spawn model-backed grandchildren unless explicitly authorized by the change owner or user.
* Never spawn a model-backed agent solely to monitor subscription usage, run `codexbar`, or perform another cheap local observation.
* Children should inherit the exact parent model route unless a specific route is intentionally configured.
* Do not silently upgrade a child to a more expensive model tier. Sol/Astra-class routes require explicit user authorization for repository development.
* If current usage is at or above the soft-stop threshold in `docs/codexbar.md`, do not start new delegated work.

### When to spawn a subagent

Consider delegating work when a task:

* can be investigated or completed independently;
* requires reading enough files, logs, tests, documentation, or external sources that isolating the working context materially helps the parent;
* involves a specialized concern such as debugging, security, performance, API research, or an independent review;
* produces substantial intermediate reasoning that the parent does not need to retain;
* can be expressed as a bounded question with a clear expected output.

Typical examples include:

* investigating a difficult failure spanning many files or logs;
* reviewing one subsystem with a clearly separate concern;
* researching an external API or dependency in depth;
* performing an intentionally independent regression/security review.

Prefer direct parent work for locating one symbol, reading a few files, running
routine tests, checking quota, applying formatting, or other tasks whose
delegation overhead is comparable to the work itself.

### Delegate bounded tasks, not entire conversations

Give each subagent the minimum context necessary to perform its assignment.

A delegation request should normally contain:

1. the concrete objective;
2. relevant files, symbols, directories, commits, or commands;
3. important constraints;
4. the expected deliverable.

Avoid forwarding the full conversation or large unrelated context unless genuinely necessary.

Prefer:

> Investigate why `FooManager.refresh()` can emit duplicate events. Inspect `src/foo/` and relevant tests. Do not modify code. Return the likely root cause, supporting file/symbol references, and a minimal proposed fix.

rather than:

> Read everything we have discussed and figure out what is wrong.

### Return compressed results

Subagents should return conclusions rather than reproducing their working context.

Unless raw output is explicitly needed, ask them to report:

* findings;
* evidence or relevant file/symbol references;
* uncertainties;
* recommended actions;
* any important risks or unresolved questions.

Avoid returning long file dumps, full command output, exhaustive search results, or detailed reasoning that the parent does not need.

The parent agent should integrate the result into its own working model and retain only the information necessary for subsequent decisions.

### Use subagents as context boundaries

Treat a subagent as an expendable working context.

A useful pattern is:

`parent identifies question -> child investigates -> child returns compact report -> parent incorporates conclusions`

The parent should not reproduce the child's entire investigation in its own context. If deeper details are later required, re-open the relevant artifact rather than automatically spawning another child.

### Decompose without automatic fan-out

For larger tasks, identify concern boundaries such as architecture, implementation,
tests, performance, security, documentation, or dependency research. Decomposition
does not imply parallel execution: run concerns serially through the parent or one
child unless concurrency has a specific, measurable benefit.

Avoid spawning multiple agents that inspect the same material without a specific reason.

### Preserve ownership

The parent agent remains responsible for:

* the overall objective;
* architectural decisions;
* subscription-budget checks and stop decisions;
* reconciling conflicting subagent findings;
* integration across components;
* final verification.

Subagents provide bounded analysis or implementation work; they should not silently redefine the task or make project-wide decisions outside their delegated scope.

### Escalate instead of expanding scope

If a subagent discovers that its task requires substantially more context or crosses an important architectural boundary, it should report this to the parent rather than recursively absorbing unrelated work.

The parent can then decide whether to:

* provide additional context;
* broaden the existing assignment;
* handle the issue directly;
* exceptionally authorize another specialized child when the benefit justifies the cost.

### Practical rule

When deciding whether to delegate, ask both:

> "Will the parent need the investigation process later, or only its conclusions?"
>
> "Is isolating this work worth another model-backed context and its quota cost?"

Delegate only when both answers favor a child. When uncertain, keep the work in
the parent.
