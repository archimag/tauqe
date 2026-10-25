# Model Roles and the Coder Ladder

One model cannot be equally good at stating intent, writing code, auditing code
written by someone else, and compressing a long transcript. TAUQE therefore splits
work by **mandate**, not by difficulty: every role keeps exactly one
responsibility, and a failure inside a role is answered by a stronger member of the
same role.

---

## 1. Motivation & Invariants ("Why")

### 1.1 The "What" versus "How" duality

Software engineering rests on two questions: *what* to build and *how* to build it.
Large language models are exceptionally strong at the second when the first is
firmly stated. Mixing the two in one undifferentiated dialogue lets a model relax a
requirement to make an implementation fit — the classic failure of autonomous
loops.

TAUQE separates the questions into distinct mandates:

- the **Architect** owns the *what*: decomposition, invariants, conceptual
  boundaries, horizon synthesis;
- the **Coder** owns the *how*: minimal, compiling changes that resolve one
  well-posed step.

### 1.2 Why roles rather than difficulty tiers

Categorizing models as *junior*, *middle*, and *senior* describes a model, not a
task. A repository needs to know which model states intent, which applies patches,
which audits findings, and which compresses history — four different jobs, often
configured with four different models. Naming the job makes the routing explicit
and auditable.

### 1.3 The invariants

1. **`primary` is mandatory** and lists at least one entry. Its first entry is the
   idle active model and the deterministic default everywhere a role is implied.
2. **An omitted specialized role inherits the entire `primary` list**, so a project
   never loses access to a configured model merely because it did not repeat the
   list.
3. **Escalation is intra-role.** A failed *how* is answered by a stronger coder
   tier, never by promoting the Coder into the Architect role.
4. **Reasoning depth belongs to a role entry; the display alias belongs to the
   catalog entry.** The same model can therefore run shallowly in one role and
   deeply in another without a duplicate catalog entry, while its display name is
   declared exactly once.
5. **The active role is resolved by the server.** A client selects a role or one
   specific model; it never guesses a role-to-model mapping.

---

## 2. Mental Model & Ontology ("What")

| Role | Mandate | Produces |
| --- | --- | --- |
| `primary` | interactive engineering dialogue | answers, develop turns |
| `scribe` | history compaction, squash commit messages, summarization | dense summaries and commit text |
| `coder` | plan steps, review fixes, verification healing | code changes |
| `architect` | plan refinement, horizon synthesis, replanning, finding decomposition | plans and subplans |
| `reviewer` | analytical, read-only inspection | review findings |

The vocabulary is small and precise:

- **`ModelRef`** — a concrete provider model (for example `~anthropic/claude-sonnet-latest`).
- **`ModelCatalogEntry`** — a model registered in the provider catalog together
  with its optional display `alias`; the single source of truth for model identity
  and naming.
- **`ModelEntry`** — a model reference (a provider identifier or a catalog alias)
  plus an optional `effort` (`low`, `medium`, `high`, `xhigh`, `max`). A bare
  string is accepted wherever an entry is accepted.
- **`CoderTier`** — one rung of the coder ladder: a model with an optional
  reasoning effort.
- **`ArchitectTier`** — one rung of the architect ladder, used to reproject a
  failing horizon.
- **`ModelSelection`** — what runs next: a `Role` (resolved by the server), a
  `Specific` model (pinned by the developer), or a `Tier` carrying the
  `{ model, effort }` pair of one exact entry of a configured role list.
- **`auto_level_up`** — the `[models]` switch that enables ladder escalation.

---

## 3. Mechanics & Lifecycle ("How it behaves")

### 3.1 Resolution and inheritance

`primary` is the only required list. `scribe` is a single utility model and falls
back to the first `primary` entry. `coder`, `architect`, and `reviewer` are ordered
lists that inherit the whole `primary` list when omitted. A specialized role
therefore always resolves to something concrete, and a configuration can grow
specialization one role at a time.

```toml
[providers.openrouter]
models = [
  { model = "~google/gemini-flash-latest", alias = "Gemini Flash" },
  { model = "~deepseek/deepseek-flash-latest", alias = "DeepSeek Flash" },
  { model = "~anthropic/claude-haiku-latest", alias = "Claude Haiku" },
  { model = "~anthropic/claude-sonnet-latest", alias = "Claude Sonnet" },
  { model = "~google/gemini-pro-latest", alias = "Gemini Pro" },
]

[models]
primary = ["Gemini Flash"]
scribe = "Claude Haiku"

# The coder list doubles as the escalation ladder.
coder = [
  "DeepSeek Flash",
  { model = "DeepSeek Flash", effort = "medium" },
  "Claude Sonnet",
]

architect = [{ model = "Gemini Pro", effort = "xhigh" }]
reviewer = ["Claude Sonnet"]
auto_level_up = true
```

A role entry names a model either by its provider identifier or by a catalog alias,
and both forms normalize to the same concrete model before a turn starts. The alias
itself is written once, beside the model in the catalog, so the picker, the headers,
and the telemetry read one declaration instead of repeating a label per role slot.

A single bare string yields a one-rung ladder; an ordered array yields an explicit
multi-rung ladder. Removing the array never breaks a configuration, it only reduces
the number of recovery rungs.

### 3.2 The coder ladder

When `auto_level_up` is enabled and a bounded turn ends with an unresolved failure
— a staging error, a verification error that healing could not repair, or a
proposal that produced no valid edits — the harness does not repeat the same
request and does not change the role:

1. The failed turn's transaction is rolled back and the transient discovery context
   is pruned, so every rung starts from the same clean pre-turn state.
2. The turn advances exactly one rung. A rung counts as an advance whenever the
   model *or* the reasoning depth changes, so recovery can first deepen reasoning
   on the same model and only then switch to a stronger one.
3. The ladder saturates on its final rung, so escalation always terminates.

A terminal blocked report short-circuits escalation entirely: no rung is consumed
and the failure is reported instead of retried. Plan step reprompting and review
finding repair use the same ladder, so recovery behaves identically wherever a
coder turn runs.

### 3.3 The architect ladder

Each horizon synthesis records the architect configuration that produced it (model
plus optional effort) on the plan, and a repeated failure is answered by a stronger
rung of the `architect` ladder. Recovery therefore first deepens the reasoning depth
on the same model and only then promotes to a stronger one, and the rung actually
used is recorded back onto the plan. Escalation saturates on the last rung; a
recorded configuration that no longer exists in configuration restarts from the base
rung instead of guessing a position.

Escalation is deliberately **lazy**. A first replan of a failing step is not
evidence that the current architect configuration is inadequate; it is only evidence
that the step's assumptions were wrong. Every horizon replacement therefore persists
the identifier of the step whose failure triggered it (`last_replanned_step` on a
plan, `subplan_last_replanned_step` on a review finding). When a step later blocks,
the replanning round compares the blocked step identity with that record:

- **a different step, or no record at all** — this is the first replan for this
  failure. The current architect configuration runs unchanged, so ordinary
  replanning never consumes a rung.
- **the same step again** — the step failed again after it was already reprojected.
  Only then does `next_architect_tier` advance the ladder, and the new rung is
  recorded back onto the plan.

The marker lives on the document rather than in the batch supervisor's memory, so a
manual replan started from the interface or through RPC follows exactly the same
rule as an automated wave, and a daemon restart cannot reset the position on the
ladder.

### 3.4 Routing

Core subsystems dispatch to a role rather than to a model name:

| Operation | Role |
| --- | --- |
| Interactive REPL turn | `primary` (or the developer's pinned model) |
| Plan step, review fix, healing | `coder` |
| Plan refinement, replanning, finding decomposition | `architect` |
| Review audit | `reviewer` |
| History compaction, squash message | `scribe` |

An interactive turn never escalates on its own: the developer's explicit selection
is always respected, and only isolated step, batch, and review repairs move along a
ladder.

### 3.5 Decision immutability

Roles exist to keep decisions stable, so the harness enforces what a role may not
do. During plan execution, conceptual invariants and finished steps are immutable:
the coder cannot relax an acceptance criterion, re-architect a boundary, or alter a
completed milestone to make a turn pass. A review finding is bound by the same rule:
the audit states what must hold, and the resolving turn implements it without
substituting the designed structure, the interfaces the finding relies on, or a
protected contract. When a requirement genuinely cannot be satisfied, the correct
answer is a blocked outcome that starts a controlled replanning cycle, not a quiet
redefinition of the task.

The prohibition is enforced on two levels, so it survives a prompt that drifts. The
`IsolatedStep` system contract states it as a rule of the environment: an isolated
turn owns the implementation only, and a conflict is reported through the blocked
outcome. The execution prompt of a plan step or a review finding restates it beside
the concrete decision, naming the structures, contracts, and reference files that
must not change. Neither level is advisory. A blocked turn consumes no coder rung,
is never retried with a weakened requirement, and reports the conflict upward: a
plan step or a review sub-step starts a replanning wave, while a finding resolved as
one turn ends `Skipped` with the blocker recorded.

### 3.6 Presentation of a model name

The name of a model is not authored by the surface that prints it. Headers,
steppers and their embedded lists, selection dialogs, and the provenance of plans
and reviews all render through one projection, so a single model can never be named
two different ways:

1. the **catalog alias**, when the catalog declares one for that model;
2. otherwise the **provider-native model name**, without the transport prefix that
   only identifies the channel.

The reasoning depth is appended next to the name in brackets, and only when the
entry actually sets one. The same model running at different depths therefore stays
distinguishable, while a model without a declared depth is printed bare. An alias
that is blank rather than absent declares nothing, so the provider name wins instead
of an empty label.

A semantic role is never part of the text. A role chooses which model runs; it does
not name it. Where a surface previously showed a role marker, it prints the concrete
entry the role resolved to, and a numbered list row derives its index from the same
projection, so the number and the name cannot drift apart.

### 3.7 Selection travels as a typed tier

Every surface that picks a model shows entries taken from a configured role list,
and such an entry carries two coordinates: the model and its reasoning depth. A
selection that named only the model would leave the second coordinate for the
server to guess, so a picked row sends `ModelSelection::Tier { model, effort }` —
the exact role-list entry the developer chose.

- **One contract, one DTO.** The primary picker, the Reviewer list of the review
  launcher, the Architect lists of the plan and review refinement dialogs, and the
  Coder list of the review-execution dialog all send the same pair. No consumer
  transports a model and reconstructs its depth afterwards.
- **The server reads the depth from the DTO.** `resolve_selection_effort` takes the
  effort straight out of a tier selection instead of scanning the configured
  entries for the first one whose model matches. The same model may occupy several
  rungs of one role list at different depths, so "the first matching entry" is not
  the entry the developer picked.
- **The established shapes keep decoding.** A `Role` still resolves through the
  role assignments reported by the server, and a `Specific` model still pins one
  concrete model and keeps the historical first-match lookup for its depth. The
  three shapes share one untagged wire form, so a stored session or an older client
  decodes unchanged.
- **The catalog row shows the full provider catalog.** The last row of the primary
  picker opens the complete list of models declared under `[providers]`, never a
  second view of the primary list. Configured primary models therefore never lock
  the developer out of a model the provider exposes, and the two lists stay
  distinguishable: one is a role list, the other is the provider catalog.
- **A picked tier never feeds a ladder.** Escalation and replanning read the
  configured ladders only. A tier selects what runs now and is never recorded as an
  escalation position.

---

## 4. Boundaries & Anti-Patterns ("What it is NOT")

- **Roles are not difficulty tiers.** A role names a responsibility, not a seniority
  level, and the same concrete model may serve several roles at different reasoning
  depths.
- **An alias is not a role parameter.** A display name describes the model and lives
  in the provider catalog; a role slot that repeats one is rejected, because two
  sources of truth for a single label drift apart.
- **A role is never part of a printed name.** A label carries the catalog alias, or
  the provider name when no alias is declared, plus the reasoning depth when it is
  set. The role appears only where the configuration itself is described.
- **Escalation never crosses the boundary.** A compile failure is a *how* problem
  and stays with the coder ladder; a failed decomposition belongs to the architect
  ladder. Neither promotes into the other.
- **A step never renegotiates the decision it executes.** A plan step and a review
  finding name what must hold; the turn implements it or reports the conflict
  through the blocked outcome. Substituting a design is a failure of the turn, not
  a decision of the coder.
- **The reviewer never writes.** The audit role produces findings only; resolution
  is a separate coder turn.
- **A role is not inferred from text.** Nothing parses a prompt to decide which
  model answers; the caller passes a typed intent and the server resolves it.
- **A selection is not a text lookup.** A picked row carries its model and its
  reasoning depth together, so no consumer re-derives a depth the caller already
  knew and no label decides which entry was meant.
- **The picker catalog is not the primary list.** The escape-hatch row presents the
  provider catalog; it is never a duplicate of the configured primary models.
- **The ladder is not an infinite retry.** It is a finite ordered list that
  saturates, so escalation has a deterministic terminus.
