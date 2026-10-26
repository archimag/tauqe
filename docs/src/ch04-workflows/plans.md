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
6. **Completed work is preserved, uncompleted work is replaceable.** Synthesis
   splices a new horizon onto the verified foundation instead of editing it.
7. **The server owns the queue.** Step statuses and advancement belong to the
   daemon, not to a client that can disconnect.
8. **A plan refuses to guess.** An unsatisfiable precondition is reported as a
   blocker, never repaired by silently weakening the requirement.
9. **A step never renegotiates its decision.** The Architect fixed *what* the step
   must achieve; the executing Coder owns only *how*. Replacing the chosen
   structure, the selected library, the designed interface, or a concept invariant
   is not a step's decision to make, and a step that cannot proceed inside the
   approved design reports the conflict.
10. **Verification is not model-configurable.** A scope is gated before its first
    turn, and each turn is verified by the project toolchain, not by a model's
    opinion.

---

## 2. Mental Model & Ontology ("What")

### 2.1 The document model

A plan is a two-section document:

| Section | Content | Mutability during synthesis |
| --- | --- | --- |
| **Concept tree** (`concept`) | nested architectural nodes: requirements, invariants, boundaries | immutable input |
| **Implementation tree** (`items`) | status-bearing steps with dependencies, concept references, and leaf files | replaceable above the `Done` foundation |

```text
plan
├── concept            # the "what": nested, never executed
│   ├── protocol shape
│   └── storage invariants
└── items              # the "how": ordered, DAG-linked, leaf-executable
    ├── [Done]        add the wire type            (done foundation)
    ├── group         persist and publish          (container, no files)
    │   ├── [Todo]    write the storage layer
    │   └── [Todo]    wire the RPC handler
    └── [Todo]        cover it with tests
```

### 2.2 Terms

- **Plan** — the persisted document: identity (`id`), `title`, `description`, the
  concept tree, the implementation tree, and the recorded Architect provenance of
  the current horizon.
- **Concept item** — a node of the concept tree; it carries intent and may nest,
  but it is never scheduled, never verified, and never binds files.
- **Plan item** — a node of the implementation tree. It carries a status, its
  prerequisites (`depends_on`), the concept nodes it satisfies (`concept_refs`),
  and — on leaves only — target files (`files`) and read-only reference files
  (`readonly_files`).
- **Leaf** — an item with no children. It is the atomic unit of a bounded turn and
  the only node allowed to bind files; any container has its file lists cleared on
  parse, on deserialize, and on save, so a container and a leaf can never disagree.
- **Done foundation** — the prefix of `Done` leaves that every synthesis preserves
  verbatim.
- **Horizon** — the uncompleted remainder of the implementation tree. It is
  replaceable as a whole.
- **Scope** — the requested execution set: one leaf, one group, or the entire plan.
- **Provenance** — the Architect configuration (model plus optional reasoning
  effort) that produced the current horizon, recorded on the plan so a later wave
  can reproduce or escalate it.

### 2.3 Status vocabulary

`Discussion`, `Todo`, `InProgress`, `Done`, `Blocked`, `Cancelled`.

Status is assigned deliberately through the status dialog, never by an accidental
keypress. `PlanItemStatus::next` defines only the cycle order of that picker and is
presentation, not permission: the authoritative guard against regressing finished
work is the storage-level immutability check, not the cycler.

### 2.4 Storage, identity, and resolution

- **One file per plan.** `.tauqe/plans/{id}.json`, so one damaged document cannot
  corrupt another.
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
| Discussion and creation | the transport configured for ordinary turns (`develop.protocol`) | the conversational message first, then an optional plan change with `action: save \| update \| delete` |
| Step execution | streaming edits | search/replace hunks plus a terminal outcome tag |

Declaring the message first lets the interface stream the rationale while the
plan payload is still arriving. A discussion touches a plan only when the
developer explicitly asks for a change; the payload carries the intent, so nothing
is inferred from the wording of a prompt. The turn renders the composite response
schema of the capability registry, so the discussion message and the plan payload
travel together in every contour and the plan change keeps one shape. Its regime
delta — the plan mutation capability that a plan discussion adds to the shared
dialogue surface — travels in the leading block of the turn's trailing message,
ahead of the plan snapshot itself, so refining the payload never moves the
byte-stable head
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
or a cycle refuses the mutation instead of persisting a partially merged plan.

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
whole before it reaches disk. A refusal is a diagnostic rather than a journal trace:
its reason is recorded in the session history and reported in the interface, so the
next request carries the diagnostic and the payload can be corrected. The harness
never repairs that payload on the model's behalf — an automatic corrective round
would rewrite a developer-owned document nobody asked it to touch — while a
successful mutation publishes `plan/updated` exactly as before.

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
- **Resilient discovery loop.** Target files of the item being refined are preloaded
  as read-only context before the first round, and those preloads bypass the files
  token budget. The loop terminates only when a round adds nothing new, reports
  nothing missing, and leaves no requested file already satisfied, so re-listing a
  file already in context is feedback rather than a dead end.
- **Explicit blocker.** Contradictory requirements or a violated convention yield a
  structured blocked result, which the interface presents together with an option to
  replan or to discuss the conflict.

Each synthesis records its provenance — the Architect model and reasoning effort —
on the plan. The Plans tree renders it as a compact `[ARC model effort]` badge, so
the document itself states which *what* role produced the current horizon.

### 3.3 Execution: scope evaluation and the server-owned queue

Execution is requested as a scope: `plan/execute` with a plan identifier and an
optional step identifier. The server evaluates the scope, owns the queue, and
advances from step to step.

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

### 3.5 The per-step bounded turn

For each scheduled leaf the server:

1. marks the step `InProgress`;
2. preloads `files` as editable context and `readonly_files` as read-only reference
   context, which removes discovery latency for the files the plan already names;
3. injects the whole concept tree and the completed milestones, and suppresses the
   conversational history so the turn reasons about the plan instead of the chat;
4. runs one streaming bounded turn, as described in *The Autonomous Bounded Turn
   FSM*;
5. marks the step `Done` only if the turn succeeded — edits applied and the
   toolchain clean — or if the model declared completion through the active
   protocol: the terminal success tag of the XML contour, or an `outcome` field with
   status `success` in the structured contour. A prose-only answer without edits and
   without a protocol completion signal does not close the step.

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
from the same clean basis instead of an accreted, contaminated one. The attempt
budget is `develop.max_retries`, which is finite by design; once it is exhausted the
step returns to `Todo` and the batch stops.

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

1. the concept tree arrives unchanged;
2. the `Done` foundation arrives unchanged, so it is never re-emitted;
3. the diagnostic packet names what failed and why;
4. the repository context supplies the freshly observed facts;
5. the resulting horizon is spliced atomically onto the foundation by
   `replace_uncompleted_horizon`, which preserves completed steps verbatim,
   disambiguates identifiers that collide with the foundation, and leaves no
   orphaned prerequisite identifiers;
6. the merged plan is validated — including dependency-cycle rejection — before it
   reaches disk, and the replanning dialogue is recorded in session history only
   after that successful persistence, so a failed attempt leaves no phantom record;
7. the batch supervisor resumes from the first executable leaf of the new horizon.

Automated recovery is bounded by `develop.max_replans`. Once consecutive waves
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
restarted on the next coder rung; files found during discovery stay in context.
Escalation never crosses the role boundary.

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
*Keyboard-First Interface*. A soft stop lets the running step finish its turn,
verification, and commit before the queue halts; a hard stop aborts it immediately.

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
- **It is not deleted silently.** A plan that still carries uncompleted steps
  leaves only through the two-tier confirmation, and both tiers default to
  `Cancel`.
- **It is not a place to regress finished work.** `Done` is immutable through every
  path except the explicit horizon clear, and the picker's cycle order is not a
  permission model.
- **It is not a local patcher.** A failed assumption reprojects the whole
  uncompleted horizon; grafting one node leaves the steps built on the broken
  assumption in place.
- **It is not an endless retry machine.** Retries, replans, and both escalation
  ladders are finite and configured.
- **It is not a design negotiation.** The architecture of a step is fixed by the
  plan. The executing turn owns the implementation and reports a conflict through
  the blocked outcome instead of substituting another design.
- **It is not a place to trade requirements for compiling code.** A blocker is the
  correct answer when a precondition cannot be satisfied.
- **It is not verified by the model.** Pre-flight and per-step verification come from
  the project toolchain, not from a model's claim of success.
- **It is not silently refused.** A mutation that fails validation names its reason in
  the session history and in the interface, and the dialogue — not a hidden retry —
  produces the corrected payload.

---

## 5. Related Chapters

- [Model Roles and the Coder Ladder](../ch01-foundations/model-roles.md) — why the
  Architect owns *what*, the Coder owns *how*, and escalation stays intra-role.
- [The Autonomous Bounded Turn FSM](../ch01-foundations/bounded-turn-fsm.md) — the
  turn a step runs, including staging, verification, and healing.
- [Three-Tier Context Model](../ch02-perception/three-tier-model.md) — what
  preloading a step's files actually changes.
- [Repository Map and Blinkers](../ch02-perception/repo-map.md) — why contours are
  not contracts during synthesis.
- [Discussion Focus](../ch02-perception/discussion-focus.md) — anchoring a dialogue
  on a plan node.
- [Git Safety & Transactions](../ch03-project/git-transactions.md) — the checkpoint a
  blocked or failed step returns to.
- [Structured Code Review](review.md) — the second workflow that produces plans.
- [Configuration and Project Preparation](../ch05-system/configuration.md) —
  `develop.max_retries`, `develop.max_replans`, and model roles.
