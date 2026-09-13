## Subagent Delegation and Context Management

Use subagents proactively to keep the primary agent's context focused and compact.

### When to spawn a subagent

Prefer delegating work when a task:

* can be investigated or completed independently;
* requires reading many files, logs, tests, documentation pages, or external sources;
* involves a specialized concern such as testing, debugging, security, performance, API research, or code review;
* produces substantial intermediate reasoning that the parent agent does not need to retain;
* can be expressed as a bounded question with a clear expected output;
* would otherwise add large amounts of low-value detail to the main context.

Typical examples include:

* locating the implementation responsible for a behavior;
* investigating a failing test or error trace;
* reviewing one subsystem or module;
* researching an external API or dependency;
* comparing several implementation alternatives;
* running tests and diagnosing failures;
* auditing a patch for regressions, security issues, or edge cases.

Do not spawn a subagent merely to repeat work already understood by the parent or for tiny tasks where delegation overhead exceeds the context saved.

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

`parent identifies question -> subagent investigates -> subagent returns compact report -> parent incorporates conclusions`

The parent should not reproduce the subagent's entire investigation in its own context. If deeper details are later required, re-open the relevant artifact or delegate another focused investigation.

### Prefer decomposition by concern

For larger tasks, split work along boundaries that minimize shared state, for example:

* architecture / design;
* implementation discovery;
* tests;
* performance;
* security;
* documentation;
* dependency or API research.

Run independent investigations in parallel when useful, but avoid spawning multiple agents that inspect the same material without a specific reason.

### Preserve ownership

The parent agent remains responsible for:

* the overall objective;
* architectural decisions;
* reconciling conflicting subagent findings;
* integration across components;
* final verification.

Subagents provide bounded analysis or implementation work; they should not silently redefine the task or make project-wide decisions outside their delegated scope.

### Escalate instead of expanding scope

If a subagent discovers that its task requires substantially more context or crosses an important architectural boundary, it should report this to the parent rather than recursively absorbing unrelated work.

The parent can then decide whether to:

* provide additional context;
* spawn another specialized subagent;
* broaden the assignment;
* handle the issue directly.

### Practical rule

When deciding whether to delegate, ask:

> "Will the parent need the investigation process later, or only its conclusions?"

If only the conclusions matter, strongly prefer a subagent.
