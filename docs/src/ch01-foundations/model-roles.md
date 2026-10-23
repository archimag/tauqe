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
4. **Reasoning depth and alias belong to an entry**, never to a model name, so the
   same model can run shallowly in one role and deeply in another without a
   duplicate catalog entry.
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
- **`ModelEntry`** — a model plus an optional `effort` (`low`, `medium`, `high`,
  `xhigh`, `max`) and an optional `alias`. A bare string is accepted wherever an
  entry is accepted.
- **`CoderTier`** — one rung of the coder ladder: a model with an optional
  reasoning effort.
- **`ArchitectTier`** — one rung of the architect ladder, used to reproject a
  failing horizon.
- **`ModelSelection`** — either a `Role` (resolved by the server) or a `Specific`
  model (pinned by the developer).
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
[models]
primary = ["~google/gemini-flash-latest"]
scribe = "~anthropic/claude-haiku-latest"

# The coder list doubles as the escalation ladder.
coder = [
  "~deepseek/deepseek-flash-latest",
  { model = "~deepseek/deepseek-flash-latest", effort = "medium" },
  "~anthropic/claude-sonnet-latest",
]

architect = [{ model = "~google/gemini-pro-latest", effort = "xhigh" }]
reviewer = ["~anthropic/claude-sonnet-latest"]
auto_level_up = true
```

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

A replanning wave must not retry the architect configuration whose horizon already
failed. Each horizon synthesis records the architect configuration that produced it
(model plus optional effort), and when a step later fails the replanning round asks
the `architect` ladder for the next rung. Recovery therefore first deepens the
reasoning depth on the same model and only then promotes to a stronger one, and the
rung actually used is recorded back onto the plan. Escalation saturates on the last
rung; a recorded configuration that no longer exists in configuration restarts from
the base rung instead of guessing a position.

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
completed milestone to make a turn pass. When a requirement genuinely cannot be
satisfied, the correct answer is a blocked outcome that starts a controlled
replanning cycle, not a quiet redefinition of the task.

---

## 4. Boundaries & Anti-Patterns ("What it is NOT")

- **Roles are not difficulty tiers.** A role names a responsibility, not a seniority
  level, and the same concrete model may serve several roles at different reasoning
  depths.
- **Escalation never crosses the boundary.** A compile failure is a *how* problem
  and stays with the coder ladder; a failed decomposition belongs to the architect
  ladder. Neither promotes into the other.
- **The reviewer never writes.** The audit role produces findings only; resolution
  is a separate coder turn.
- **A role is not inferred from text.** Nothing parses a prompt to decide which
  model answers; the caller passes a typed intent and the server resolves it.
- **The ladder is not an infinite retry.** It is a finite ordered list that
  saturates, so escalation has a deterministic terminus.
