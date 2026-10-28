# Discussion Focus

Planning and review are analytical activities. The developer asks why a design is
shaped a certain way, weighs trade-offs, and refines requirements. A focus binds
that dialogue to one plan or one review session, so follow-up questions stay
grounded without re-selecting the entity or switching screens.

---

## 1. Motivation & Invariants ("Why")

### 1.1 Analysis and mutation are different regimes

A Develop turn carries the full editing toolchain: the model may request files,
stage patches, run verification, and rewrite the working tree. A discussion must
not do that by accident. When analysis and mutation share one untyped channel, two
failures follow: the model answers a design question by editing files the developer
only wanted to inspect, and requirements drift silently.

### 1.2 Two failure modes

- **Unintended mutation.** The answer mutates the tree because the turn was a
  normal Develop turn with editing enabled.
- **Accidental mode entry.** If the mode were inferred from prompt text, a message
  that merely resembles a discussion heading could enter discussion mode, and
  identifiers read out of prose could address the wrong entity.

### 1.3 The invariants

1. **The target is typed.** The mode and the addressed entity travel as typed
   request fields; nothing is inferred from the body of a prompt.
2. **Edits follow a direct request.** Both dialogue contours run on the main
   capability surface of an interactive turn, and the authorization rule — not the
   contour — decides whether a file changes: a file is edited only when the
   developer asks for that change directly.
3. **A review focus stays actionable.** It keeps the interactive editing surface on
   purpose, so the same dialogue can reason about a finding and apply its fix.
4. **The anchor survives a plain ask.** While a focus is active, every ordinary
   message is answered against the live state of the focused entity.
5. **The developer owns status changes.** A discussion explains a proposed change;
   it never rewrites persisted review or plan state in the background.
6. **A stale anchor degrades gracefully.** Deleting the addressed plan releases
   its anchor explicitly, and any entity that cannot be loaded drops the anchor
   and answers the raw prompt instead of failing the turn.
7. **Detaching is guarded.** Releasing the anchor is an explicit, confirmed act.

---

## 2. Mental Model & Ontology ("What")

### 2.1 Target

`DiscussionTarget` identifies the addressed entity and nothing else:

- `Plan { id }` — a plan, addressed by plan identifier.
- `Review { id }` — a review session, addressed by session identifier.

### 2.2 Focus

`DiscussionFocus` is the active anchor:

- `None` — free dialogue.
- `Focused { target, focused_items }` — dialogue anchored to one entity and,
  optionally, to specific items inside it.

`focused_items` carries plan step and concept identifiers for a plan, and the
decimal string form of finding identifiers for a review session.

### 2.3 Terms

- **Discussion request** — typed target, focused items, and a free-form comment.
- **Quick focus** — setting the anchor from Plans or Review without leaving the
  view or typing a prompt.
- **Stale anchor** — a focus whose entity no longer exists.
- **Regime profile** — the two typed capability lists a regime declares: a main
  surface rendered into the byte-stable head and shared by every dialogue, and a
  delta rendered into the tail.

### 2.4 Intent mapping

| Target | Turn intent | Capability surface |
| --- | --- | --- |
| `Plan { id }` | `PlanDiscussion` | the main dialogue surface plus the plan-mutation delta |
| `Review { id }` | `InteractiveRepl` | the main dialogue surface, with an empty profile delta |

The mapping is a typed decision, not a text check. It is the single place that
decides which regime a focused dialogue runs in: both targets reach the same
editing surface, and only the plan target adds the capability that persists a
plan change.

---

## 3. Mechanics & Lifecycle ("How it behaves")

### 3.1 Opening a focused discussion

An explicit discussion request carries the target, the focused items, and the
developer's comment. The server loads the entity, generates the discussion prompt
from those typed fields, replaces the focus anchor, and derives the turn intent.

Opening a focus does not reset the REPL session history: the developer keeps the
running dialogue while switching the entity under discussion.

### 3.2 Prompt composition

Both prompts share a shape: a heading naming the entity, its architecture or scope,
the live entity state inside a machine-readable block, the focused items, the
developer comment, and explicit instructions.

A plan discussion renders the concept tree and the implementation items inside
`<active_plan>`, marks focused nodes, and restates two rules: never mutate the plan
`<active_plan>`, marks focused nodes, and restates two rules: never mutate the
plan without an explicit instruction, and express plan changes through a typed
plan payload (`save`, `update`, `delete`) with a language directive taken
from the plan itself.

A review discussion renders findings inside `<active_review>`, including the defect
description, ordered recommendations, decomposition plan, resolution note, and the
protected contracts listed in `readonly_files`. Its instructions keep the turn
actionable: apply requested code changes directly, treat protected contracts as
immutable, and explain status changes instead of applying them.

A discussion turn uses the transport the project configures for ordinary turns
(`develop.protocol`). Whatever the contour, the prompt asks for the response
schema of the active capability profile, so the conversational message and the
plan payload travel together and a plan change keeps one shape in every
transport. The response declares the message first, which lets the interface
stream the answer while the rest of the payload is still arriving.

### 3.3 Capability surface

A dialogue runs on the main capability surface of an interactive turn — text,
discovery, documentation requests, file edits, verification, scratchpad, and the
terminal outcomes — and each regime adds its own delta. A plan discussion adds
the plan-mutation delta (`PlanUpdate`), which is what lets the same anchored
dialogue refine a plan and persist the change. A review discussion adds no
capability of its own today, so its delta is empty and its surface is exactly the
main one.

Because the main surface carries file edits, a focused dialogue reaches exactly
the Develop transaction: edits are staged, verified, and committed as one atomic
turn, and a failure rolls the tree back to the pre-turn checkpoint.

### 3.4 Continuing, replacing, and dropping the anchor

- **Continue.** A plain ask does not clear the anchor. The server rebuilds the
  prompt from the current state of the focused entity, so a follow-up question is
  answered against the live plan or session rather than a stale snapshot.
- **Replace.** An explicit target replaces the anchor without resetting history.
- **Stale.** If the entity cannot be loaded (for example, it was deleted
  elsewhere), the server drops the anchor, logs the failure, and answers the raw
  prompt instead of failing the turn.
- **Deleted entity.** Deleting the addressed plan releases the anchor on the
  server before the deletion response is written: the focus becomes `None` and the
  `discussion/focusChanged` event carries the release to every client. A client that
  instead observes the anchored plan disappear from a plan list refresh drops the
  anchor locally, so the header and the REPL banner can never name a plan that no
  longer exists. A review anchor is untouched by a plan deletion.
- **Clear.** Detaching returns the dialogue to free mode. Clearing the
  conversation history also detaches the focus, so a fresh session never inherits a
  dangling anchor.

### 3.5 Interface indication and quick focus

While a focus is active, the REPL reserves one status line directly above the
prompt. The banner names the entity with a `[PLAN]` or `[REVIEW]` badge, its
identifier and title, the number of targeted items, and a short hint for releasing
the focus. It carries no command of its own and is carved out of the response area,
so it never competes with the prompt for space.

Both Plans and Review bind `f` to quick focus on the selected row. On a plan header
it anchors the whole plan; on a step or finding it anchors that item. The command
dispatches `discussion/set_focus` immediately, keeps the view unchanged, confirms
the anchor with a notification, and marks the focused row with a `[FOCUS]` badge.

### 3.6 Safe detach

Detaching is bound to the abstract primary command chord (`C-g`, `Ctrl` by default)
in the REPL and is guarded by a confirmation dialog whose default focus is
`[ Cancel ]`. The guard exists because detaching is easy to trigger and silently
discards the explicit grounding of the conversation.

When no focus is active, the chord is inert and the interface simply reports that
there is nothing to detach. `Esc` is deliberately not the detach key: in the REPL
it keeps its long-standing meaning — interrupt a running turn, or clear the prompt
on a second press — so the focus reset never competes with the most-used key on the
keyboard.

### 3.7 Status changes stay developer-owned

When a discussion concludes that a plan item or review finding should change
status, the model states the proposed modification in its answer. The developer
applies it through the interface. Persisted plan and review state is never rewritten
as a side effect of a conversation, which keeps the Reins invariant intact: durable
state moves only on an explicit developer decision or a verified execution.

---

## 4. Boundaries & Anti-Patterns ("What it is NOT")

- **It is not an agent mode.** A focus grounds a dialogue; it does not grant a new
  workflow.
- **It is not an unguarded plan editor.** A plan discussion may produce a
  structured update, but only when the developer explicitly instructs one, and the
  storage engine still enforces plan immutability.
- **It is not a status mutator.** Status changes are explained and applied by the
  developer.
- **It is not inferred from text.** No prefix, heading, or keyword enters
  discussion mode.
- **It is not a history reset.** Setting or replacing a focus preserves the running
  dialogue.
- **It is not a screen switch.** Quick focus binds an entity without leaving Plans
  or Review.
- **`Esc` is not detach.** The pause-and-clear meaning of `Esc` is preserved.
- **A focused dialogue is not a second editing path.** It reuses the Develop
  transaction — staging, verification, and the Git rollback — and never opens a
  dialogue-specific write path.

---

## 5. Related Chapters

- [The Context Model](context-model.md) — the working set a focused
  dialogue inspects.
- [Local Plans](../ch04-workflows/plans.md) — plan structure, refinement, and
  execution.
- [Structured Code Review](../ch04-workflows/review.md) — findings, statuses, and
  resolution.
- [The Autonomous Bounded Turn FSM](../ch01-foundations/bounded-turn-fsm.md) —
  `TurnIntent` and capability narrowing.
- [Keyboard-First Interface](../ch05-system/tui-ux.md) — the focus banner and
  dialog conventions.
