# Core Engine Conventions

These are review criteria for model-facing execution, not a map of control
modules or a specification of today's wire fields.

## Model-facing contracts

- Model operations use a typed, constrained output contract. A response's
  advertised capabilities, schema, instructions, decoder, and stream filter
  must agree. A disabled capability must not be accepted through a different
  decoding path or appear as an available instruction.
- Keep operational branches mutually exclusive. A model-produced change must
  not be silently treated as a request for more context or as an answer.
  Reject malformed structure rather than recovering intent from raw text.
- Keep user-facing text separate from operational metadata and private
  scratchpad content. Stream only text intended for the user; do not expose
  protocol scaffolding or private notes through previews or history.
- Consumers act on typed commands, never by scraping a provider's JSON or
  guessing a mode from prompt text. Keep model-facing types independent of
  client RPC types.
- Support configurable edit formats and application policies. A change of
  default must not erase an explicitly selected supported strategy. The engine
  chooses and passes policy; deterministic patch matching and filesystem/Git
  operations remain with the substrate.

## Bounded execution

- Reuse one bounded multi-round execution mechanism for tasks with the same
  lifecycle. Scenario-specific authorization, interpretation, scheduling, and
  persistence must not leak into the mechanism merely because two scenarios
  call it.
- Charge budgets at the operation they limit. A rejected request must not
  consume a provider-call budget, and a retry within one admitted call must
  not masquerade as a new turn round.
- Distinguish success, legitimate alternative outcomes, exhausted attempts,
  cancellation, and infrastructure failure. Never discard an unhandled
  command at the top-level boundary or continue a dirty transaction as if a
  failure were a semantic refusal.
- Hard cancellation must be observed across the whole provider operation,
  including the wait for headers and optional completion work. Optional
  enrichment must not hold up a delivered result.
- Do not perform blocking filesystem, Git, or toolchain work on an async
  worker thread. Keep synchronous work cancellable at its orchestration
  boundary where possible.

## Context, prompts, and history

- A round must see a coherent version of each file it is asked to edit.
  Preserve earlier delivered text in a turn transcript; communicate subsequent
  changes explicitly rather than silently replacing what the model saw.
- Keep per-turn research and transcript state distinct from durable session
  history. Persist only the information needed to explain outcomes and support
  later turns, not private scratchpad text or raw protocol payloads.
- Assemble repeated prompt material deterministically. Legitimate new
  discoveries and feedback should extend the transcript without rewriting
  earlier bytes unnecessarily; volatile material should not invalidate a
  reusable prefix by accident.
- Bound context and history by declared budgets. Do not hide a context-limit
  failure by truncating material whose completeness is required for a decision.
- Admit the complete serialized model request against its shared input limit.
  Audit window expansion does not impose a separate file allowance or drop
  readable matches to make an oversized request appear admissible.
- A turn that is cancelled, blocked, or rolled back must not pretend it
  committed changes. Keep recoverable research available until its lifecycle
  is explicitly settled.

## Observability

- Emit semantic events for significant transitions and failures without
  coupling the engine to a logging sink or UI.
- Do not log prompts, file bodies, generated deltas, credentials, or full
  diffs during normal execution. Use compact diagnostics and identifiers.
