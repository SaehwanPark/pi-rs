# Repository Agents Guide

## What

- `rupi` is a minimal, Pi-inspired coding-agent runtime implemented in Rust.
- The project is a Rust 2024 workspace delivering a minimal, fast core with
  honest provenance, recoverable state, and behavioral Pi compatibility.
- `docs/PROJECT_DESIGN_CANONICAL.md` is the canonical design authority.
  `ROADMAP.md` is the canonical project plan and progress tracker.
- `ARCHITECTURE.md` records verified runtime boundaries, subsystem interfaces,
  and architecture invariants. `COMPATIBILITY.md` defines Pi behavioral
  compatibility targets and fixtures.
- `CONTRIBUTING.md` specifies operational developer workflows, verification
  commands, and pull-request slicing.
- Upstream Pi is a behavioral reference, not an architecture to translate
  mechanically; this is a clean reimplementation, not a fork.

## Why

- Preserve Pi-like minimalism at the user-facing layer while keeping domain
  workflows and higher-level orchestration outside core runtime.
- Maintain honest provenance: canonical trace remains separate from
  model-visible context; never conflate native reasoning, provider summaries,
  declared rationale, and reconstructed rationale; never claim hidden
  chain-of-thought was recovered unless actually exposed.
- Maintain single-model execution: only one model is active in normal
  execution; backup model activation is fault recovery, not orchestration.
- Preserve explicit state: uncertain mutating tool operations (`write`, `edit`,
  `exec`) remain `Unknown` and are never blindly replayed.
- Aggressively protect startup latency (<100 ms warm, <250 ms cold): keep
  MCP, extension host, backup models, and deep session hydration lazy behind
  adapter boundaries.

## How

- Before substantial changes, read `docs/PROJECT_DESIGN_CANONICAL.md`,
  `ARCHITECTURE.md`, `COMPATIBILITY.md`, and `ROADMAP.md`.
- Use the delivery harness in `docs/harness/rupi-development/team-spec.md`
  and specialized skills:
  - `.agents/skills/rupi-change-orchestrator/SKILL.md` for slice ownership
    and synthesis;
  - `.agents/skills/rupi-core-contract-designer/SKILL.md` for typed state
    and runtime contracts;
  - `.agents/skills/rupi-compatibility-fixture-author/SKILL.md` for Pi
    behavioral compatibility fixtures;
  - `.agents/skills/rupi-invariant-reviewer/SKILL.md` for cross-cutting
    invariant review.
- Use spaces with an indentation and tab width of 2, 100-column line limit, and
  idiomatic stable Rust. Run checks before review:

  ```sh
  cargo fmt --all --check
  cargo check -p rupi-core --all-features
  cargo clippy --workspace --all-targets -- -D warnings
  cargo test --workspace
  cargo doc --workspace --no-deps
  bash bench/startup.sh --json bench/results/startup-ci.json
  ```

- Changes affecting startup, rendering (`bash bench/render.sh`, budgets in
  `crates/rupi-tui/benches/render.rs`), session resume, or context
  reconstruction must verify performance against defined budgets.
- Update `ROADMAP.md` only from verified evidence. Keep incomplete work active and do not mark stage gates complete until gate evidence exists.
- Be aware of AI subscription usage limits and reset windows. The parent/current
  agent checks limits directly per `docs/ai-usage-policy.md`; never spawn a model-backed
  subagent solely to monitor usage.
- Use `llama.cpp` to serve AI models locally when necessary to examine how `rupi` performs with local models. See `docs/local-ai-serving.md` for details.

## Subagents

Stay single-agent by default. Delegate only when the context, specialization, or
parallel-latency benefit clearly outweighs the additional model-turn cost.

* Default to at most one active model-backed child. Use two concurrently only for clearly independent work with an explicit benefit and healthy usage headroom.
* Delegate bounded, self-contained investigation or implementation tasks when the parent mainly needs the result, not the working process.
* Prefer a child for unusually read-heavy or specialized work, not as the default way to run tests, inspect a few files, monitor quota, or perform routine verification.
* Give subagents only the context and scope needed for their task; avoid copying the full parent conversation unless necessary.
* Ask subagents to return concise findings, evidence/references, risks, and recommended actions rather than raw working context.
* Keep architectural decisions, cross-component integration, final verification, and usage monitoring with the parent agent.
* Do not recursively spawn model-backed grandchildren unless the user or change owner explicitly authorizes that topology.
* Do not silently escalate a child to a more expensive model tier. Inherit the exact parent route or use an explicitly configured same/lower-cost route. Sol/Astra-class routes require explicit user authorization for repository development.
* Avoid redundant subagents inspecting the same scope unless independent review is intentional.
* If a subagent's scope expands substantially, it should escalate back to the parent rather than absorbing unrelated work.
* Use the main context for decisions; use subagent contexts only where they provide a clear net benefit.

See `docs/subagents_policy.md` for detailed delegation patterns and guidance.

## Asynchronous GitHub Communication

Use GitHub proactively as the durable communication channel when human collaborators are unavailable or work may continue across sessions.

* **Always strongly** prefer remote branches, commits, PRs, and GitHub discussions/comments over keeping important state only in local context.
* Push meaningful work to a remote branch regularly when it is safe and useful to preserve progress.
* Open a draft PR early for non-trivial work when it provides a useful place for status, design notes, review, and human steering.
* Keep PR descriptions and comments updated with current status, key decisions, unresolved questions, risks, and next steps.
* Use commits and PRs to leave a durable trail that another human or agent can resume without reconstructing the full conversation.
* When blocked on a human decision, record the question and relevant context in the PR or issue rather than leaving it only in transient agent context.
* Prefer small, reviewable commits and branches with clear scope.
* If code is reviewed before opening PR, skip the review step and open the PR directly.
* Do not merge, close, force-push shared work, or perform other irreversible repository actions unless explicitly authorized or clearly permitted by project policy.
* Never commit secrets, credentials, private data, or machine-specific sensitive artifacts.
* Make the project development progress **monitorable via GitHub**. To do so, make sure remote branches are up-to-date with the latest local branches during development. **Do not wait** until the end of development to push to remote branches.

Use local context for active reasoning; use GitHub for durable project state and asynchronous human communication. Make the project owner can monitor the agent's current progress/status accurately via GitHub.

## Agentic Loop

Use agentic loops for long-running tasks or when pursuing goals, but keep each
loop bounded by both a target slice and the current subscription budget.

One loop is defined by

1. Check current usage and select one target slice (what to implement/examine/do)
2. Design a plan
3. Execute the plan
4. Test and verify
5. Update documents if necessary
6. PR handoff and merge autonomously
7. Re-check usage, then move to the next slice only when below the soft-stop threshold

Do not use autonomous continuation as a reason to consume the remaining budget.
When the soft-stop threshold is reached, leave durable GitHub state and a concise
handoff rather than beginning another slice.
