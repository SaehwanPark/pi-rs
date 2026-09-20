# rupi rebrand and v0.2.1 release brief

## Intent

Rebrand the released coding harness from `pi-rs` to `rupi` because the old name implies a
rewrite or Rust port of Pi. Update the public repository identity, package/binary names,
state/config examples, user documentation, screenshots, and release metadata while
preserving the explicitly supported upstream Pi compatibility surfaces (`Pi`, `.pi`, and
Pi package/session formats).

## Bounded slices

1. **Identity migration:** rename the GitHub repository, Cargo workspace/packages and
   directories, binary/config/state identifiers, environment variables, skills, paths,
   URLs, CI, and tests; leave upstream Pi terminology unchanged where it describes the
   compatibility contract.
2. **Public docs and local-model examples:** make the GitHub Pages guide beginner-first,
   use the supplied llama.cpp OpenAI-compatible endpoint (`127.0.0.1:8000`, model
   `qwen3.8-flash-next`, native reasoning), and regenerate representative screenshots
   from a real local request.
3. **Documentation curation:** keep current contracts and user guidance in the root;
   move resolved audit/slice/proposal material under explicit archives, remove stale
   duplicate/generated material, and repair links/indexes.
4. **Test curation:** inventory tests by contract and boundary, remove only duplicate
   assertions that prove the same observable behavior, consolidate repetitive CLI cases
   into focused table-driven tests where it improves readability, and retain one
   representative success plus the important refusal/failure/recovery boundaries. Do
   not weaken provenance, durability, failover, trust, confinement, or compatibility
   coverage. Record the reduced suite size/runtime and run the full workspace checks.
5. **Release:** bump all first-party packages to `0.2.1`, update changelog/site metadata,
   tag and publish the GitHub release after the WIP PR is reviewed by CI.

## Acceptance evidence

- `rg` finds no stale first-party `pi-rs`/`PI_RS`/`pi_rs` identity outside historical Git
  history or intentional upstream-Pi compatibility wording.
- `cargo metadata`, `cargo fmt`, `cargo check`, `cargo clippy`, `cargo test`, `cargo doc`,
  and `mdbook build book` pass with the new package and binary names.
- The local Qwen3.8-Flash-Next endpoint answers a smoke prompt through `rupi`; the
  captured public examples identify the real model/endpoint and do not claim invented
  latency or reasoning provenance.
- The curated guide works for a first-time coding-harness user: install, configure,
  run, approve/reject a mutation, resume, and recover from common errors are explicit.
- A draft PR is opened early, progress is pushed in coherent commits, and the final
  authorized merge/release actions are recorded in the PR and changelog.

## Non-goals

- Do not remove Pi compatibility or rename upstream `.pi` conventions.
- Do not rewrite Git history or delete audit evidence merely because it is old; archive
  resolved material with an index and preserve provenance.
- Do not add a new test framework or speculative runtime behavior.
- Do not claim a hosted Pages URL or release asset until the corresponding GitHub action
  or release command succeeds.
