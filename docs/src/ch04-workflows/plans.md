# Local Plans

A local plan is TAUQE's tactical unit of engineering work: a durable document that
separates *what* must be built from *how* it is built, and that survives session
restarts, history compaction, and a change of model. Plans live in `.tauqe/plans/`,
inside the Git-ignored runtime directory, so no plan can ever reach a commit or a
branch history.

---

## 1. Motivation & Invariants ("Why")

### 1.1 A backlog is not an execution contract

Repository documentation states durable intent, and it is the right place for a
roadmap. Tactical work is a different object: a protocol addition, a schema
migration, or a subsystem refactor needs an ordered set of steps that a harness can
drive one bounded turn at a time. A flat todo list fails that job three ways — it
cannot express dependencies, it cannot separate architecture from implementation,
and it does not survive the conversation that produced it, because a compacted
transcript fades and instructions inside it fade with it.

### 1.2 Why the *what* / *how* split is structural, not stylistic

Planning fails in one of two directions. When intent and implementation live in one
undifferentiated list, a model that cannot implement a step quietly reinterprets the
requirement to fit the code it managed to write. When intent is never written down at
all, every step is re-derived from a shrinking conversation, and consecutive steps
disagree about boundaries.

A plan therefore holds two trees. The **concept tree** states requirements,
invariants, and scope boundaries once, and is treated as immutable input during
synthesis. The **implementation tree** is the ordered, executable decomposition of
that intent. The Architect owns the first, the Coder owns the second, and *Model
Roles and the Coder Ladder* owns that boundary. A plan is where it becomes visible.

### 1.3 The invariants

1. **Storage is local and ignored.** One file per plan under `.tauqe/plans/`, never
   committed, never visible as repository noise.
2. **Concept and implementation are separate trees.** Requirements are not steps,
   and steps do not redefine requirements.
3. **Only leaves execute, and only leaves bind files.** A parent node is a
   milestone or a phase; it is never a unit of work.
4. **Dependencies form a DAG.** A step starts only when every prerequisite is
   `Done`; cycles are rejected before a plan reaches disk.
5. **`Done` is immutable.** A finished step cannot be deleted, renamed, have its
   files changed, or be regressed to an earlier status.
6. **A step enters existence as work, never as execution.** A plan mutation
   may create a step only as `Todo` or `Discussion`. `InProgress`, `Blocked`,
   and `Cancelled` belong to the execution pipeline and to the developer's
   manual control, while `Done` is reachable only through a verified step turn.
7. **Completed work is preserved, uncompleted work is replaceable.** Synthesis
   splices a new horizon onto the verified foundation instead of editing it.
8. **The daemon owns the queue.** Step statuses and advancement belong to the
   server-side pipeline, not to a client that can disconnect.
9. **A plan refuses to guess.** An unsatisfiable precondition is reported as a
   blocker, never repaired by silently weakening the requirement.
10. **A step never renegotiates its decision.** The Architect fixed *what* the step
    must achieve; the executing Coder owns only *how*. Replacing the chosen
    structure, the selected library, the designed interface, or a concept invariant
    is not a step's decision to make, and a step that cannot proceed inside the
    approved design reports the conflict.
11. **Verification is not model-configurable.** A scope is gated before its first
    turn, and each turn is verified by the project toolchain, not by a model's
    opinion.
12. **The step graph is one shared structure.** A plan, an architectural review
    group, and a single review finding execute the same `PlanImplementation`, so
    readiness, aggregation, leaf normalization, and dependency validation are
  defined once and cannot drift between workflows.
13. **A step declares the external specifications it obeys.** A leaf names the declared
    `[[outer_docs]]` entries its implementation is checked against, and the harness mounts
    them before its turn starts, so the specification arrives without a research round.
14. **A refusal is typed where it is born.** A refused plan mutation carries the category
    its producer assigned — a defective payload, a forbidden regime, or an abandoned
    payload — and no layer derives that category from the wording of a reason.
15. **A payload is never lost silently.** A turn settles its mutations through one phase
    at finalization, and a turn that closes without a qualifying result abandons its
    payload with a typed diagnostic instead of dropping it.

---

## 2. Mental Model & Ontology ("What")

### 2.1 The document model

A plan is a two-section document:

| Section | Content | Mutability during synthesis |
| --- | --- | --- |
| **Concept tree** (`concept`) | nested architectural nodes: requirements, invariants, boundaries | immutable input |
| **Implementation horizon** (`implementation`) | the step graph (`items`) with dependencies, leaf files, and decomposition provenance | replaceable above the `Done` foundation |

```text
plan
├── concept            # the "what": nested, never executed
│   ├── protocol shape
│   └── storage invariants
└── implementation     # the "how": the step graph and its provenance
    ├── items          # ordered, DAG-linked, leaf-executable
    │   ├── [Done]     add the wire type            (done foundation)
    │   ├── group      persist and publish          (container, no files)
    │   │   ├── [Todo] write the storage layer
    │   │   └── [Todo] wire the RPC handler
    │   └── [Todo]     cover it with tests
    ├── architect      # the model and effort that produced this horizon
    └── last_replanned_step
```

`implementation` is a plain nested field, and `PlanImplementation` is the single owner
of the step graph and of its provenance. Every graph query is a method of that
structure — readiness by dependency, aggregate status, leaf progress, leaf-only file
normalization, structural validation — so no consumer restates the algebra and no two
workflows can disagree about what a ready step is.

The nested form is the only accepted shape. The field carries neither `serde(flatten)`
nor `serde(default)`, so a document written in the former flat layout fails to decode
instead of being silently read as a plan without steps. There is no migration path, and
the legacy `subplan*` names of a review group are not resurrected either (*Structured
Code Review* §2.1).

### 2.2 Terms

- **Plan** — the persisted document: identity (`id`), `title`, `description`, the
  concept tree, and the nested `implementation` that owns the step graph together with
  the recorded Architect provenance of the current horizon.
- **Step graph (`PlanImplementation`)** — the nested structure holding `items`, the
  `architect` configuration that produced the horizon, and `last_replanned_step`. It is
  the single source of truth for every graph query, and a review group owns the same
  structure, so a plan step and a group step are evaluated by one algebra.
- **Concept item** — a node of the concept tree; it carries intent and may nest,
  but it is never scheduled, never verified, and never binds files.
- **Plan item** — a node of the implementation tree. It carries a status, its
  prerequisites (`depends_on`), the concept nodes it satisfies (`concept_refs`),
  and — on leaves only — target files (`files`), read-only reference files
  (`readonly_files`), and the external documents the step obeys (`outer_docs`).
- **Leaf** — an item with no children. It is the atomic unit of a bounded turn and
  the only node allowed to bind files; any container has its file lists cleared on
  parse, on deserialize, and on save, so a container and a leaf can never disagree.
- **External specification binding** — the `outer_docs` field of a leaf, naming declared
  `[[outer_docs]]` entries rather than paths. The harness materializes and mounts every
  named document read-only before the step turn starts, so a specification the step must
  satisfy arrives without a research round (*External Project Documentation*).
- **Done foundation** — the prefix of `Done` leaves that every synthesis preserves
  verbatim.
- **Horizon** — the uncompleted remainder of the implementation tree. It is
  replaceable as a whole.
- **Scope** — the requested execution set: one leaf, one group, or the entire plan.
- **Provenance** — the Architect configuration (model plus optional reasoning effort)
  that produced the current horizon, recorded as `implementation.architect` together
  with `implementation.last_replanned_step`, so a later wave reproduces or escalates
  that configuration instead of re-deriving one.
- **Mutation rejection** — a typed refusal of one plan mutation: its category
  (`PayloadDefect`, `ModeForbidden`, or `Abandoned`) plus the wording a surface renders
  verbatim. The category is born with the refusal and is never inferred from its text.
- **Settlement phase** — the single phase every terminal path of a bounded turn runs its
  accumulated plan payload through before packing the outcome. It applies the mutations
  or abandons them, and it reports the repairs the status barrier performed.
- **Normalization warning** — the report of a status the barrier repaired instead of
  refusing: the step was persisted, and the status it requested was replaced by the one a
  step is allowed to carry when it enters existence.

### 2.3 Status vocabulary

`Discussion`, `Todo`, `InProgress`, `Done`, `Blocked`, `Cancelled`.

A step enters existence only as `Todo` or `Discussion`. The model authors
plans but never runs them, so the remaining statuses belong to other
owners: `InProgress`, `Blocked`, and `Cancelled` are assigned by the
execution pipeline and by the developer's manual control, while `Done` is
reachable only through a verified step turn. A payload that asserts any
other status on a step it creates has the assignment repaired to `Todo`
and the repair reported, so the mutation still applies and the deviation
stays visible instead of silently rewriting the plan.

Status is assigned deliberately through the status dialog, never by an accidental
keypress. `PlanItemStatus::next` defines only the cycle order of that picker and is
presentation, not permission: the authoritative guard against regressing finished
work is the storage-level immutability check, not the cycler.

### 2.4 Storage, identity, and resolution

- **One file per plan.** `.tauqe/plans/{id}.json`, so one damaged document cannot
  corrupt another.
- **Canonical nested shape.** The horizon travels under `implementation` with its
  provenance, in the single form described in §2.1. A file written in the former flat
  layout is reported as a decode failure rather than migrated, so a stale document never
  reaches execution as a plan without steps.
- **Active pointer.** `.tauqe/plans/active` records the last inspected plan, so a
  client reconnect restores the developer's context. It is a view pointer only and
  never steers a mutation.
- **Slug identity.** Identifiers are short slugs (`jwt-auth`, `tui-refactor`) and
  are resolved typo-tolerantly through Levenshtein distance and prefix matching, so
  a minor misspelling in a request or in a discussion resolves to the intended plan
  instead of failing the turn. An ambiguous match is reported as ambiguous rather
  than guessed.

### 2.5 Two protocol contours

Planning and execution are different regimes and use different wire contours:

| Regime | Protocol | Shape |
| --- | --- | --- |
| Discussion and creation | the transport configured for ordinary turns (`repl.protocol`) | the conversational message first, then an optional plan change with `action: save \| update \| delete` |
| Step execution | streaming edits | search/replace hunks plus a terminal outcome tag |

Declaring the message first lets the interface stream the rationale while the
plan payload is still arriving. A discussion touches a plan only when the
developer explicitly asks for a change; the payload carries the requested change,
so the action is never inferred from the wording of a prompt. The turn renders
the composite response schema of the capability registry, so the discussion
message and the plan payload travel together in every contour and the plan change
keeps one shape.

Plan mutation belongs to exactly one turn regime. Whatever the contour, decoding
narrows the advertised set to the capability surface of the declared intent: a
dialogue anchored on a plan is mapped onto `TurnIntent::PlanDiscussion` and gains
the plan-mutation delta that a plan discussion adds to the shared dialogue
surface, while an interactive dialogue carries the dialogue head without plan
mutation, whatever focus anchor is active. Asking for a new plan is not mutation
— plan creation stays in the dialogue head, so a developer may request a plan at
any moment of a dialogue, while changing an existing plan requires the
plan-discussion regime. The delta travels in the leading block of the turn's
trailing message, ahead of the plan snapshot itself, so refining the payload
never moves the byte-stable head
([The Autonomous Bounded Turn FSM](../ch01-foundations/bounded-turn-fsm.md)).

### 2.6 The two mutation actions

A `plan_update` payload reaches storage under one of two contracts, and the
distinction is deliberate:

- **`save` replaces.** It writes the document as a whole: identity, description, the
  concept tree, and the implementation tree. Because the replacement is total, every
  step that already stands at `Done` must be reproduced verbatim — its status, title,
  details, files, readonly_files, concept_refs, and depends_on — or the
  completed-step immutability check rejects the mutation. A text-contour save that
  carries the concept tree alone keeps the established steps instead of erasing them.
- **`update` patches.** It addresses nodes by identifier: an existing step or concept
  node is amended, an unknown concept node is appended, and an unknown step is
  appended to the plan root as a leaf. Adding a leaf is the delta counterpart of the
  concept upsert, which lets one `update` both refine established work and extend the
  horizon. Restructuring — introducing containers, moving a subtree, changing the
  hierarchy — remains the exclusive province of `save`.

Neither action bypasses the guards: the merged document is validated as a whole, so a
repeated identifier, a dependency that does not exist, an unknown concept reference,
or a cycle refuses the mutation instead of persisting a partially merged plan. Both
actions share one feedback contract: a refusal is typed at the barrier that produced it,
and the turn reports it instead of letting the mutation disappear (§3.13).

---

## 3. Mechanics & Lifecycle ("How it behaves")

### 3.1 Creating and refining the concept

A plan normally begins in dialogue. The developer states the objective in Develop or
discusses a specific node with `d`, and the model answers with rationale plus a
`plan_update` payload. Concept nodes are shaped there, while the work is still
cheap to redefine. A plan whose concept tree is stable and whose implementation tree
is still empty is a complete, valid artifact: it declares *what* without committing
to *how*.

Planning is two phases, and the harness keeps them apart.

**Phase one — the proposal.** A broad request arrives in Develop: work that touches
several independent subsystems or contracts, that decomposes into dependent stages,
or whose stages must be verified separately. The turn answers in plain text, states
briefly why decomposition is warranted, and offers the developer a choice between
drafting a plan and performing the whole change at once. The proposal is text only —
no plan document reaches storage, and no edit block is emitted in the same answer.
The turn still closes with its terminal success tag, so the offer is a completed
turn rather than an abandoned one.

**Phase two — the creation.** The document is written only after the developer
confirms the offer or asks for a plan explicitly. That answer arrives through the
`plan_update` payload described above and reaches disk as an ordinary plan save. A
declined offer is equally binding: if the developer asks for the whole task instead,
the model executes it completely and never repeats the proposal.

Mechanical and localized work never enters this fork. A cross-cutting rename, a
signature update, or one focused bug fix is compact and indivisible, so it is
executed immediately instead of being proposed for decomposition.

**Applying the payload.** Whatever the action, the merged document is validated as a
whole before it reaches disk, and a refusal is a typed diagnostic rather than a journal
trace: its category and its wording are recorded in the session history and reported in
the interface. A payload that is itself defective buys one corrective exchange, in which
the model re-emits the document and the harness validates only what arrives (§3.13). The
harness never rewrites the payload on the model's behalf — the corrected document is
always authored by the dialogue — while a successful mutation publishes `plan/updated`
exactly as before.

### 3.2 Horizon synthesis (deep refinement)

Pressing `r` (or `R` for every item awaiting decomposition) dispatches the Architect
to produce or reproject the implementation tree. The pass runs isolated from the
conversational history and honours five rules:

- **Concept immutability.** The Architect receives the concept tree as an immutable
  requirement and may not rephrase, relax, or delete it. Its mandate is the
  implementation tree only.
- **Done foundation preservation.** Completed steps are passed in as verified input
  and are never re-emitted, so a synthesis cannot regress or erase them.
- **Contracts over contours.** The repository map shows names and shapes, never
  exact type layouts, trait bounds, lifetimes, or error semantics. Whenever a
  synthesized step must interact with an existing item, the Architect requests the
  concrete source files and returns no steps for that round. A contract guessed from
  a contour line is a violation.
- **Declared external contracts.** A step whose implementation must satisfy a document the
  project does not own names that document in `outer_docs`, so the executing turn receives
  the specification instead of searching for it. The name must be a declared `[[outer_docs]]`
  entry, because the harness mounts the entry rather than a path the step supplies.
- **Resilient research loop.** Target files of the item being refined are preloaded
  as read-only context before the first round, and those preloads bypass the files
  token budget. The loop terminates only when a round adds nothing new, reports
  nothing missing, and leaves no requested file already satisfied, so re-listing a
  file already in context is feedback rather than a dead end.
- **Explicit blocker.** Contradictory requirements or a violated convention yield a
  structured blocked result, which the interface presents together with an option to
  replan or to discuss the conflict.

The pass is not a loop of its own. It runs on the shared turn kernel, which owns
the rounds, the byte-stable prompt head, the rendered repository outline, the
research journal, and the corrective budget of an empty answer, while the
structured Architect contour owns what is specific to a horizon. The domain
framing of a synthesis — the plan identity, the immutable concept, the `Done`
foundation, a blocked step, and the developer directives — travels in the task
fragment of the user message, so the protocol states the response contract and
the kernel renders the material of each round.

A payload that violates the response schema never reaches storage. Schema
Healing returns the decoding diagnostic to the model as the trailing directive
of the next request and spends one generation attempt of the running rung,
whose budget the rung declares itself. Only an exhausted rung climbs the
architect ladder, and the promoted rung applies its own reasoning effort to
every later round and rotates the provider session identifier with the
effective model, so a cached prefix is never reused across a model change.
The synthesis therefore ends in a typed status instead of in prose: a
synthesized horizon, a blocker that names the contradiction, an abort that
names the host action only the developer can perform, an exceeded context
budget, or a cancellation. An abort is not a blocker: it states that no plan
can complete the work in the current environment, so the harness streams the
exact command live and ends the synthesis on that verdict without spending a
generation attempt of the running rung. A saturated ladder ends it as a
blocker, never as a fabricated horizon, and an empty `items` list beside a
success status is refused as a protocol defect. The interface sees the pass
while it
runs: the contour reports its rounds, its promotions, and its outline size as
kernel events, and the server bridge turns them into the turn-phase,
model-escalation, and outline telemetry a client already renders.

Each synthesis records its provenance — the rung that actually produced the
horizon, model plus reasoning effort — on the plan, together with the step whose
failure triggered a replanning wave. The Plans tree renders the rung as a
compact `[ARC model effort]` badge, so the document itself states which *what*
role produced the current horizon.

### 3.3 Execution: a typed scope and one shared pipeline

Execution is requested as a scope: `plan/execute` with a plan identifier and an
optional step identifier. The request resolves to `ExecutionScope::Plan { plan_id }`,
one variant of the closed enumeration that names every source of a step graph — a
local plan, an architectural review group, and a single review finding. The set is
closed on purpose: exactly three workflows own a horizon, each resolves by identifier
from the storage that owns it, and a caller never supplies a horizon of its own.

The resolved frame is the stored plan itself. From there the pipeline owns the queue:
`PlanImplementationPipeline` schedules the ready leaves over the shared `BatchExecution`
supervisor, `PlanImplementationStepExecutor` drives one leaf, and every graph decision
— readiness by dependency, stall classification, persistence of a status — reads and
writes the storage that owns the horizon. Review groups and findings reach the same
machinery through their own scope variant (*Structured Code Review* §3.7), so a step
graph never acquires a private scheduler.

**The turn seam.** The engine owns *what* a step does; the daemon owns *how* one turn
of it executes. That boundary is one trait, `ImplementationHost`, with exactly two
operations: `run_step` executes one bounded step turn and classifies its terminal
outcome, while `run_refinement` executes one Architect wave that reprojects a horizon.
Provider telemetry, usage windows, streamed events, and Git transactions live behind
the seam and are never observed by the engine — the same rule that governs
`ApplyPolicy` and `ProviderResolver`.

Evaluation decides the outcome before anything runs:

- a step targeted while in `Discussion` is refused, because human triage has not
  happened yet;
- a step explicitly targeted while `Cancelled` or `Blocked` is refused, while a
  broad scope simply skips such terminal states;
- every scheduled prerequisite in `depends_on` must already be `Done`, otherwise
  the scope is refused as blocked by dependencies;
- a scope whose steps are all `Done` reports that everything is complete.

A plan that carries only its concept tree is deliberately *not* reported as fully
complete: its scope is an empty queue that the supervisor projects just in time, so
pressing `x` on a purely architectural plan is never rejected as having nothing to
execute.

**Why the queue belongs to the server.** Marking a step `InProgress`, running its
turn, and marking it `Done` must be one atomic decision tied to the verified result
of that turn. A client-owned queue would lose the batch on a disconnect and let two
clients disagree about plan state. The interface is therefore a thin view: it starts
the scope and renders `plan/updated`, `plan/listChanged`, `turn/phase`, and `model/*`
events.

### 3.4 Pre-flight verification

Before the first step runs, the server executes the same toolchain pipeline the core
runs after every step. A repository that does not build or whose tests fail is
refused synchronously — before any model request, before any status change, and at
zero token cost — with the compiler or test output attached. Repositories without a
detected toolchain skip the gate transparently.

The gate runs while the operation slot stays registered, and it inherits that
operation's cancellation channel. A hard cancel or a shutdown drain therefore tears
the verification subprocess group down instead of waiting the build out, and the
slot is released within a bounded interval: the cancellation is answered when the
teardown completes, not when the suite would have finished. The gate reports that
outcome as a cancellation, never as a verification failure of a suite that never
ran to completion.
### 3.5 The per-step bounded turn

For each scheduled leaf the pipeline persists the step as `InProgress` in the storage
that owns the horizon and hands one turn request to the seam. The daemon executes it:

1. it clears the transient context layer a previous step left behind, then preloads
   `files` as editable context and `readonly_files` as read-only reference context, and
   materializes the documents `outer_docs` names, which removes research latency for the
   material the plan already declared;
2. it runs one streaming bounded turn under `TurnIntent::IsolatedStep`, as described in
   *The Autonomous Bounded Turn FSM*, with the whole concept tree and the completed
   milestones injected and the conversational history suppressed, so the turn reasons
   about the plan instead of the chat;
3. it classifies the terminal outcome into the failure taxonomy: a semantic block
   travels as its own verdict so the Architect engages instead of the coder ladder, an
   unconditional abort stops the queue without spending a wave, a change set that was
   never rolled back is an unreverted failure, and a turn that produced no model result
   is an infrastructure failure rather than a content verdict.

Before the turn starts, the harness resolves the leaf's `outer_docs` and mounts every named
document read-only, priced like any other context entry under `[context].token_budget`. A
trust refusal is not the step's failure: only the developer can widen a declaration the
repository cannot reach, so the step is never started and the run stops with the exact
cause and the `[[workspace]]` block that would authorize the source. A delivery failure — a
name the declaration does not carry, an unreachable host, an HTTP status, a timeout, a
cancelled transfer — is a diagnostic of the step prompt instead: the turn starts, and the
model routes around the missing document, which may become reachable again in a later turn.
A budget breach is reported the same way.

The pipeline then closes the lifecycle: the step becomes `Done` only if the turn
succeeded — edits applied and the toolchain clean — or if the model declared completion
through the active protocol: the terminal success tag of the XML contour, or an
`outcome` field with status `success` in the structured contour. A prose-only answer
without edits and without a protocol completion signal does not close the step.

A step reports the phases of its turn while it runs. The turn phase, the
toolchain start, and the toolchain finish reach the interface as they happen,
exactly as they do for an interactive turn, so a minute-long verification shows
the phase that is running instead of a screen that waits for the batch event
which closes the step.
Injecting the whole concept tree is what makes the decision explicit to the
executing model, and it is backed by two independent statements. The step prompt
names the structures, libraries, interfaces, and concept invariants the step must
not replace, and requires a blocked outcome when the step cannot be implemented
inside them. The `IsolatedStep` system contract states the same prohibition as a
rule of the environment, so the constraint does not depend on the step prompt
remaining intact.

When every child of a container is `Done`, the container is recalculated to `Done`.
While a scope runs, the REPL shows a compact Plan Stepper: the plan title and one
title-only row per step in scope with a short status. It never expands diffs or
concept nodes, and it is absent for ad-hoc turns. The Proposed Edits area belongs
strictly to the *current* step: once the batch advances, the previous step's files
are already committed and are dropped from the working area.

### 3.6 Reprompt loop and deterministic reset

A bounded turn can fail the step contract conversationally: it answers in prose,
forgets its edit block, or omits the terminal tag. Treating that drift as a hard
batch failure wastes the whole scope, so the server separates *insurmountable*
failures — terminal error tag, cancellation, exhausted budget — from *recoverable*
protocol drift.

For the latter it re-runs the same step with a corrective prompt that echoes the
previous answer, includes the diagnostic reason, and restates the active edit
protocol in that protocol's own vocabulary: the XML contour asks for a valid edit
block or the terminal success tag, while the structured contour asks for a `changes`
array or an `outcome` with status `success`. Conversational text is never accepted in
place of a protocol operation.

Between attempts the basis is reset deterministically: the failed turn's own file
changes are rolled back by the Git workflow checkpoint, the transient auto context
is cleared, and the stable step files are re-loaded. Each attempt therefore starts
from the same clean basis instead of an accreted, contaminated one. A reset that did
not actually return the tree to the checkpoint is never retried over: the step
reports the failure instead of generating against a working tree that still carries
the previous attempt, because the next round would reason about code it did not
write.

The reprompt budget is the per-tier budget, `repl.max_tier_attempts` (3 by
default), and not an independent counter. The step is re-asked on the same coder rung
while that budget lasts, and only an exhausted tier lets `models.auto_level_up`
advance the ladder, where the next rung receives a fresh budget of its own. A
saturated ladder returns the step to `Todo` and stops the batch. The staging retries
and healing rounds inside one attempt stay bounded by `repl.max_retries`, which is
a different limit and escalates nothing.

The terminal tag carries a dynamic per-turn marker. A marked tag is preferred, and
an unmarked terminal tag outside Markdown code fences is accepted as a fallback when
no marked tag is present, so a literal example in the answer is never mistaken for a
real signal. That marker is minted only while the XML contour is active, because its
sole purpose is to bind tagged commands: a structured turn generates none, and its
completion signal is the typed `outcome` field rather than a tagged spelling.

### 3.7 Semantic blocking and wave-horizon replanning

Real work invalidates assumptions. When a step discovers that a requirement is
contradictory, an interface does not exist, or a third-party dependency is broken,
the correct answer is a blocker, not speculative drift. The model emits the blocked
tag, the harness intercepts it immediately, rolls the working tree back to the
pre-turn checkpoint, skips patch retry and healing entirely, and records the step as
`Blocked`. This is the Decision Immutability protocol rather than a failure path: a
blocked step is never retried on the same prompt, never escalated along the coder
ladder, and never repaired by relaxing the requirement it could not satisfy.

Repairing the horizon is deliberately *not* local subtree grafting. A failed
assumption at step *N* also invalidates the steps designed around it, so retaining
downstream items introduces orphaned dependencies and broken contracts. TAUQE
replaces the uncompleted horizon as a whole:

```text
Horizon = Architect(Concept, DoneFoundation, RepositoryContext, Diagnostics)
```

The wave itself travels through the seam: the pipeline sends a refinement request that
names the failed step, its diagnostic, the Architect configuration of the wave, and the
developer's directives, and `run_refinement` answers with a synthesized horizon or with
a blocker. A synthesized result then reaches storage as one operation:

1. the concept tree arrives unchanged;
2. the `Done` foundation arrives unchanged, so it is never re-emitted;
3. the diagnostic packet names what failed and why;
4. the repository context supplies the freshly observed facts;
5. the resulting horizon is spliced atomically onto the foundation by
   `replace_uncompleted_horizon`, which preserves completed steps verbatim,
   disambiguates identifiers that collide with the foundation, and leaves no
   orphaned prerequisite identifiers, while the same write records the rung in
   `implementation.architect` and the failed step in
   `implementation.last_replanned_step`;
6. the merged plan is validated — including dependency-cycle rejection — before it
   reaches disk, and the replanning dialogue is recorded in session history only
   after that successful persistence, so a failed attempt leaves no phantom record;
7. the batch supervisor resumes from the first executable leaf of the new horizon.

Automated recovery is bounded by `repl.max_replans`. Once consecutive waves
exceed that budget the batch halts with the step still `Blocked` and asks for the
developer.

**Lazy architect ladder escalation.** A replanning wave must not retry a
configuration whose horizon already produced the failing step, but it also must not
escalate on the first sign of trouble. Every horizon replacement therefore persists
the identity of the step whose failure triggered it as `last_replanned_step` on the
plan, and the wave compares that record with the blocked step:

- **a different step, or no record at all** — this is the first replan of this
  failure. The current architect configuration runs unchanged, so ordinary
  replanning never consumes a rung.
- **the same step again** — the step failed again after it was already reprojected.
  Only then does the wave read the provenance recorded on the plan and ask the
  `architect` ladder for the next rung, so recovery first deepens the reasoning
  depth on the same model and only then promotes to a stronger one.

The marker lives on the document rather than in the batch supervisor's memory, so a
manual replan started from the interface or through RPC follows exactly the same
rule as an automated wave, and a daemon restart cannot reset the position on the
ladder. Escalation saturates on the last rung, and a recorded configuration that no
longer exists in configuration restarts from the base rung instead of guessing a
position.

**Coder ladder escalation** is the parallel, intra-role mechanism for execution
failures (*Model Roles and the Coder Ladder*). With `models.auto_level_up` enabled,
a step that still fails after healing is rolled back to its pre-step checkpoint and
re-asked on the same coder rung, up to `develop.max_tier_attempts` attempts; files
found during discovery stay in context. Only an exhausted tier advances one rung, and
the new rung starts with a fresh budget of its own. Escalation never crosses the role
boundary, and it saturates: the last rung is the last attempt.

### 3.8 Just-in-time horizon synthesis

A concept-only plan does not have to be decomposed before it can run. When a batch
reaches a plan whose implementation tree is empty but whose concept tree is
established, it launches one Architect synthesis round before the first step, stores
the horizon atomically, and feeds its leaves into the same supervisor loop. The
operation slot is never released and the batch is never restarted, so the
developer's single `x` press remains the whole authorization.

### 3.9 Safe horizon clearing

`Shift+C` discards the implementation horizon while preserving the architectural
concept: identity, title, and the whole concept tree stay untouched, and only the
recorded provenance is dropped because the decomposition it described no longer
exists.

This is the one deliberate bypass of the `Done` guard — an explicit developer
authorization to remove every step, not a silent regression. Everything else stays
protected by the storage-level immutability check.

The confirmation is two-tiered, driven by `has_started_or_completed_steps`:

- every leaf still `Todo` — one confirmation dialog;
- any leaf `Done` or `InProgress` — a strict two-tier modal, where the second tier
  warns that the plan will diverge from committed repository history.

Both tiers default focus to `Cancel`. A plan whose implementation tree is already
empty emits an informative notice instead of opening a dialog.

### 3.10 Plan pipeline

`X` starts `plan/executePipeline`: an ordered queue of plans executed one after
another under a single registered operation.

- Each plan is decomposed or refined only *after* the previous plan finished, so the
  Architect reasons about the actual Git tail and the current repository map instead
  of a stale capture.
- Concept-only plans compose naturally, because their horizon is projected just in
  time before the queue reaches them.
- One operation slot and one set of cancel channels cover the whole pipeline, so a
  cancellation delivered between plans is honoured rather than lost.
- A plan that ends `Failed`, `Blocked`, `Cancelled`, or `Stopped` halts the queue
  before the next plan starts: downstream plans depend on a repository state the
  failed plan was supposed to produce.
- Exactly one typed batch-finished event reports the final status, the executed
  versus scheduled counts, and the elapsed time, which is what drives the aggregated
  notification.

Cancellation is three-way (`Continue`, `Soft Stop`, `Hard Stop`), as specified in
*Keyboard-First Interface*, and both stop degrees act inside the running step
rather than after it. The queue is awaited cooperatively: the supervisor hands the
cancellation and soft-stop channels to the step and keeps awaiting it, instead of
racing the queue against a cancellation branch and dropping the unfinished future.
A hard cancel observed while a step runs therefore unwinds the queue through that
step itself — the provider races the signal at every await and the toolchain tears
its verification subprocess group down at the cancel boundary — and the operation
slot is never released while a spawned task is still running. Dropping the future
instead would detach the task the step had already spawned and release the slot
together with its abort handle, leaving an unsupervised task holding the session
lock and possibly an open Git transaction. Within the step the channel is read at
every phase boundary that starts work — before the check, between healing rounds,
before the staged result is committed — so a soft stop halts at the nearest
boundary instead of letting the turn run to its natural end. The interrupted step
is never marked `Done` and never committed: the existing checkpoint transaction
returns the working tree to its pre-turn state.

A cancel that no running phase observes on its own is resolved by the
deferred-abort watchdog, the sanctioned force-termination path: it defers the abort
while the task still holds an open Git transaction and aborts once that transaction
has closed. The operation registration therefore outlives every task spawned under
it, and the slot stays registered for as long as one of them runs.

### 3.11 Plans view commands

| Command | Action |
| --- | --- |
| `n` / `p` (also `↑` / `↓`) | move through plans, concept nodes, and steps |
| `Home` / `End` (`Alt+<` / `Alt+>`) | first / last row |
| `PgUp` / `PgDn` | page through the tree |
| `Ctrl+N` / `Ctrl+P` | accordion walk to the next / previous node |
| `Tab` / `Space` | fold or unfold the selected section or node |
| `a` | fold or unfold every plan and section |
| `Enter` / `x` | execute the selected leaf, group, or plan, with confirmation |
| `X` | start the plan pipeline over every ready plan |
| `r` / `R` | hand the selected item / every item awaiting decomposition to the Architect for horizon synthesis |
| `d` | discuss the selected concept node, step, or the whole plan in Develop |
| `t` / `s` | open the status dialog |
| `Shift+C` | clear the implementation horizon, concept preserved |
| `c` / `y` | copy the plan as Markdown |
| `g` | re-read plans from server storage |
| `Delete` | delete the plan, with confirmation |
| `Ctrl+H` / `?` | open contextual help |

### 3.12 Safe plan deletion

Deleting a plan destroys task context that no Git commit restores: the concept
tree, the horizon, the recorded statuses, and the provenance of the current
decomposition exist only in `.tauqe/plans/`. `Delete` is therefore guarded by a
confirmation whose default focus is `Cancel`, and the strength of that guard
follows the work the plan still carries.

The tier is chosen before the dialog opens:

- **every leaf finished or nothing left to do** — all executable leaves are `Done`
  or `Cancelled`, or the plan is still concept-only and has no horizon at all. One
  confirmation names the plan, its derived status, and states that every step is
  completed.
- **any leaf uncompleted** — at least one leaf is `Todo`, `InProgress`, or
  `Blocked`. A second tier restates the remaining count against the total, switches
  to the dangerous tone, and warns that the recorded progress is lost for good.

The switch is the authoritative leaf accounting of `Plan::count_stats`, not the
derived status of a container. That metric already excludes cancelled leaves, so a
plan that was finished and then partially cancelled is not treated as unfinished
work, while a `Blocked` leaf — the one status that explicitly asks for developer
triage — always escalates.

Deletion is a daemon mutation: the client dispatches `plan/delete` and rebuilds its
plan cache from `plan/listChanged` instead of patching the list in place. Deleting
the plan a discussion is anchored on also releases that anchor, so no interface
keeps naming a plan that no longer exists (*Discussion Focus* §3.4).

---
### 3.13 Mutation feedback: typed refusals, one repair round, and report surfaces

Applying a payload is a dialogue act with a verdict, not a fire-and-forget write. Three
properties keep that verdict legible: the refusal is typed where it is produced, the
payload settles exactly once at the end of the turn, and every outcome — applied,
repaired, refused, or abandoned — reaches both the model and the developer.

**Typed refusals.** A refusal is born typed, as `PlanRejection { kind, message }`, and the
category belongs to the code that produced it:

| Kind | Producer | Meaning |
| --- | --- | --- |
| `PayloadDefect` | plan storage | the payload cannot be applied: the planning contract or the structural validation of the stored plan refuses it — a parse that yielded no usable document, a missing concept tree, an `update` whose identity names no plan or several plans, an `update` that tries to introduce a container, or a `save` that fails validation |
| `ModeForbidden` | plan storage | the turn regime forbids mutation whatever the payload carries, as an isolated step does |
| `Abandoned` | the turn engine | the payload never reached storage, because the turn closed without a qualifying result |

The message travels beside the category and is the wording a surface renders verbatim. No
lower layer and no client derives a category from the text of a reason (root conventions
§1, *Typed Intent over Textual Inference*), so a refusal keeps the classification of the
barrier that made it all the way to the client that shows it.

**One settlement phase at finalization.** Every terminal path of a bounded turn runs the
accumulated payload through the same settlement phase exactly once, before the outcome is
packed; finishing a turn no longer applies mutations as a side effect. The phase reads a
declared qualification:

- a **qualifying** result — a validated answer or an applied change set — applies every
  mutation and collects the warnings the status barrier produced;
- a **non-qualifying** result — a semantic block, an abort, a terminal error, a patch
  failure, an edit proposal that applied nothing, an empty round, or an exceeded context
  budget — abandons the payload with a single `Abandoned` refusal whose message names the
  reason, and records a warning in the journal. The budget case is deliberate rather than
  exceptional: a turn that exhausted its budget without widening its context is exactly
  the turn whose payload must not reach storage unverified.

An empty payload settles into nothing, so a turn that carried no mutation reports no
diagnostic. The single intentional exception is a turn the developer cancelled: that work
ended by explicit request, so the abandoned payload is not reported as a defect.

**One repair round for payload defects.** `PayloadDefect` is the only category generation
can correct, so it buys exactly one corrective exchange — never a loop, and never a
configuration knob. The typed reasons compose a directive that travels through the turn's
research journal, and one proposal round follows. That answer is absorbed partially: its
plan payload and its text, and nothing else. The terminal semantics of the turn are not
reopened, so a repair round cannot turn an answer into a block, cannot edit files, and
cannot widen the turn it repairs. Only the payload of the repair round is settled
afterwards, so mutations the first batch already applied are never re-executed, because
replaying them would manufacture a refusal for work that already succeeded. `ModeForbidden`
and `Abandoned` spend no generation at all: repeating a turn whose regime forbids the
mutation, or resending a payload that already left the turn, cannot change the verdict.

**Normalization is reported.** The status barrier repairs a requested status instead of
refusing the payload: a step enters existence as `Todo` or `Discussion`, and a new step
that asserts any other status is persisted with the assignment repaired and the repair
reported. That repair is a lifecycle event of the plan, not an implementation detail, so it
leaves storage as `plan_warnings` on the turn outcome. Silent normalization is forbidden:
the model and the developer both see that the requested status was corrected.

**Report surfaces.** Four surfaces carry the verdict, and none stands in for another:

| Surface | Carries | Consumer |
| --- | --- | --- |
| Turn banner | the rejection and warning notices rendered as the turn closes | the developer watching the turn |
| Session history | the diagnostic recorded with the turn | the durable record of the dialogue |
| Journal | one warning per refusal, repair, and abandonment | post-mortem analysis of the daemon |
| `ModelResultEvent` | `plan_rejections` and `plan_warnings`, filled by the server from the turn result | the client, which raises one warning notification with a localized heading and the refusal messages as its detail |
## 4. Boundaries & Anti-Patterns ("What it is NOT")

- **It is not a repository artifact.** Plans are local, ignored, and never
  committed; durable project intent belongs in documentation.
- **It is not created by the proposal that suggests it.** Offering to plan and
  writing the plan are separate phases; the document reaches storage only after the
  developer authorizes it explicitly.
- **It is not a conversation transcript.** A plan is a structured document that
  survives compaction, so an instruction that must persist belongs in the plan, not
  in the chat.
- **It is not a graph the client schedules.** The daemon owns statuses and
  advancement; the interface only starts a scope and renders events.
- **It is not an owner of its own executor.** A plan, an architectural review group, and
  a single review finding execute the same pipeline over the same step graph, so a
  workflow never acquires a private scheduler.
- **It is not a legacy format to migrate.** A plan document is read only in its
  canonical nested shape, so the harness never guesses a horizon out of a stale layout.
- **It is not deleted silently.** A plan that still carries uncompleted steps
  leaves only through the two-tier confirmation, and both tiers default to
  `Cancel`.
- **It is not a place to regress finished work.** `Done` is immutable through every
  path except the explicit horizon clear, and the picker's cycle order is not a
  permission model.
- **It is not a local patcher.** A failed assumption reprojects the whole
  uncompleted horizon; grafting one node leaves the steps built on the broken
  assumption in place.
- **It is not an endless retry machine.** Retries, replans, per-tier attempts, and
  both escalation ladders are finite and configured.
- **It is not a design negotiation.** The architecture of a step is fixed by the
  plan. The executing turn owns the implementation and reports a conflict through
  the blocked outcome instead of substituting another design.
- **It is not a place to trade requirements for compiling code.** A blocker is the
  correct answer when a precondition cannot be satisfied.
- **It is not verified by the model.** Pre-flight and per-step verification come from
  the project toolchain, not from a model's claim of success.
- **It is not silently refused.** A mutation that fails validation names its reason in
  the session history and in the interface, and one corrective exchange — not a hidden
  retry loop — lets the dialogue produce the corrected payload.
- **It is not classified by wording.** The category of a refusal is born with the refusal,
  so no layer guesses a cause from the text of a reason.
- **It is not a payload that outlives its verdict.** A turn that closes without a
  qualifying result abandons its mutations with a typed diagnostic instead of applying
  them silently, and a cancelled turn is the only silence the contract allows.
- **It is not normalized in silence.** A status the barrier repairs is reported through
  the same surfaces as a refusal, because a repaired plan and a rejected payload are both
  decisions the developer must see.

---

## 5. Related Chapters

- [Model Roles and the Coder Ladder](../ch01-foundations/model-roles.md) — why the
  Architect owns *what*, the Coder owns *how*, and escalation stays intra-role.
- [The Autonomous Bounded Turn FSM](../ch01-foundations/bounded-turn-fsm.md) — the
  turn a step runs, including staging, verification, and healing.
- [The Context Model](../ch02-perception/context-model.md) — what
  preloading a step's files actually changes.
- [Repository Map and Blinkers](../ch02-perception/repo-map.md) — why contours are
  not contracts during synthesis.
- [Discussion Focus](../ch02-perception/discussion-focus.md) — anchoring a dialogue
  on a plan node.
- [Git Safety & Transactions](../ch03-project/git-transactions.md) — the checkpoint a
  blocked or failed step returns to.
- [Structured Code Review](review.md) — the second workflow that produces plans, and
  executes them through the same pipeline.
- [Configuration and Project Preparation](../ch05-system/configuration.md) —
  `repl.max_retries`, `repl.max_replans`, and model roles.
