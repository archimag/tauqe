# Development Conventions

These rules guide engineering decisions and code review. They are not an inventory
of the current implementation. Architecture documents and code describe how a
rule is realized; tests enforce behavior that can be checked automatically.

## Language and clarity

- Write repository artifacts, source comments, identifiers, commit messages,
  and the default user interface in plain technical English. Localized
  interfaces may display other languages through the localization system.
- Prefer direct explanations and explicit names over jargon or incidental
  metaphor. Explain the reason for a non-obvious constraint near its owner.

## Engineering decisions

- Make the smallest coherent change that meets the requirement. Do not add
  speculative features, modes, layers, or compatibility paths without a
  current consumer.
- Give each rule, format, and state transition one authoritative owner. Reuse
  that owner rather than copying its logic into a new subsystem.
- Never infer typed intent from prose or guess facts that Git, parsers,
  manifests, or compiler output can establish deterministically.
z- Use maintained ecosystem implementations of established standards instead
  of writing private substitutes, unless a documented constraint prevents it.
- Make boundaries explicit: callers supply intent and policy; lower layers
  return typed outcomes rather than silently choosing a workflow. Keep
  presentation, domain decisions, transport, and deterministic operations
  separate when their responsibilities differ.
- Delete superseded behavior rather than leaving dormant alternatives. Preserve
  compatibility only where an actual external or persisted contract requires it.

## Safety and verification

- Treat developer data, repository state, and external inputs as untrusted until
  admitted by the responsible boundary. Validate paths before writes and
  preserve pre-existing work on failure or cancellation.
- Validate changes before committing them. A failed verification must not be
  disguised as success; distinguish a domain refusal from an infrastructure
  failure and report enough information to recover safely.
- Do not substitute a stub or fallback when a required host capability is
  missing. Report the missing prerequisite for the developer to resolve.
- Keep tests with the behavior they protect. Prefer contract tests at layer
  boundaries to assertions about private implementation details.

## Workspace boundaries

These project-specific boundaries are the exception to the general decision
rules above. They constrain where new work may be placed:

- `protocol` contains only the wire contract for interaction with the TUI. It
  does not own domain rules or model-facing command protocols.
- `server` is a thin RPC adapter around application and engine behavior. It
  owns transport, logging setup, final error translation, and wire projection;
  it does not implement the workflows it exposes.
- `tui` owns presentation and interaction, not authoritative application or
  persisted state. Client projections may compute display and optimistic
  previews, but cannot authorize durable transitions.
- The division of responsibility among `tools`, `core`, and `flow` is being
  refined. Do not create an inverse dependency or duplicate a rule to make a
  move easier; establish the owner of a behavior before relocating it.

Crate-specific conventions below define additional reviewable constraints for
their domains. Current module maps, algorithms, and RPC routes belong in code
and architecture documentation, not in these conventions.
