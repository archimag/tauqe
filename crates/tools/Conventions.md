# Deterministic Substrate Conventions

These rules govern technical operations performed without model judgment.
The caller supplies intent and policy; the substrate reports typed results.

## Ownership and admission

- Do not interpret model output, select a workflow, or choose a project command
  here. Accept resolved inputs from the caller and keep technical results
  independent of transport and UI types.
- Derive repository facts from Git, filesystem metadata, and parsers rather
  than from model narration. Keep shared pure utilities free of application
  state so every consumer uses the same matching semantics.
- Reject protected, ignored, escaping, occupied, or otherwise inadmissible
  paths before publishing changes. Check both source and destination of a
  move. A refusal must identify what failed without silently widening access.

## Change sets and persistence

- Parse and stage each supported edit format in memory. Do not write to disk
  until the proposed change set and its formatting have been validated.
  Failed application must not expose a partially accepted patch document.
- A retry must start from the accepted staged state and must not leak effects
  of a refused member. Preserve bytes for moves, including non-text files;
  restrict text-only transformations to valid text.
- Apply committed changes atomically and durably. On a failure, restore
  previously published paths and remove newly created artifacts. Recovery
  must not remove an artifact still owned by a live writer.
- Keep formatting inside virtual staging. A formatter failure must not rewrite
  the working tree or be mistaken for successful verification.

## External processes and Git

- Invoke declared programs with literal argument vectors, not through an
  interpretive shell. Bound child execution, cancel and reap the entire
  process group, and avoid signalling a reused process identity.
- Protect developer changes with a checkpoint before a transactional write.
  Do not reset history, clear another writer's transaction, reclaim a lock, or
  delete a staging artifact without sufficient ownership and revision proof.
- Sanitize Git's repository-redirection environment so a subprocess operates
  on the repository explicitly supplied by the caller. Prove ignore protection
  before staging a commit.
- Return typed, compact diagnostics; never substitute a successful outcome
  when Git, a formatter, or a toolchain invocation cannot be verified.

## Deterministic discovery

- Generate repository outlines from tracked files and parsers, not model
  descriptions. Make ordering reproducible and stay within the caller's
  budget. Do not follow links outside the admitted repository surface.
- Keep blocking disk and process work off async workers. An uncertain offload
  outcome must not cause the same side effect to run twice.
