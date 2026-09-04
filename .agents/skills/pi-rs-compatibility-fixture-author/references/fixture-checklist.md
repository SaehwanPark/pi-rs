# Compatibility Fixture Checklist

Select only the relevant surface.

## Every Surface

- Record the targeted Pi behavior or version family.
- Prefer documented behavior over assumptions about internals.
- Test deterministic discovery and stable diagnostics.
- Include malformed or unsupported input when users could encounter it.
- Keep the fixture self-contained and free of network dependence.
- Explain any expected lossy conversion.

## Skills

- standard `SKILL.md` structure
- project-local and package-local discovery as applicable
- YAML frontmatter and user-visible semantics
- relative references and bundled resources
- no Node host when the skill is data-only

## Prompt Templates

- name and discovery location
- variable behavior
- missing-variable diagnostic
- content preserved independently of provider implementation

## Packages

- manifest parsing and discovery
- install path behavior
- contained skills/prompts/extensions exposed per surface
- one unsupported optional surface reported without rejecting supported surfaces
- dependencies handled only when required

## Sessions

- message ordering and model/tool metadata
- import and export behavior
- explicit warning for provenance or event data that cannot round-trip
- no weakening of the internal session/event model
- malformed and future-version handling

## Extensions

- tool or command registration before broader lifecycle/UI behavior
- Node host starts only when executable extension behavior is required
- host failure does not corrupt the Rust session
- unsupported internal imports are diagnosed
- extension input/output is normalized at RPC boundaries

## Themes and UI

- preserve semantic intent rather than pixel/widget parity
- keep pi-rs-only provenance and rare-event states distinguishable
- unsupported complex custom UI behavior is explicit
