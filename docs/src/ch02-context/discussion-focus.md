# 2.8. Discussion Focus: Isolated Architectural Dialogue

Planning and review are analytical activities: the developer asks *why* a design
is shaped a certain way, weighs trade-offs, and refines requirements. They are
distinct from a Develop turn, whose purpose is to change code. TAUQE keeps the
two apart through a dedicated **discussion focus**: a typed anchor that binds the
dialogue to one plan or review session and routes every turn through a
read-only, structured protocol.

This section explains why that isolation exists, how the anchor is addressed and
carried, what the model may emit while the focus is active, and how the focus is
shown and safely released in the interface.

## 2.8.1. Why Discussion Needs Isolation

A normal Develop turn carries the full editing toolchain: the model may request
files, stage patches, run verification, and rewrite the working tree. A
discussion must not. When analysis and mutation share one channel, two failure
modes appear:

- **Unintended mutation.** The model answers a design question by editing files
  the developer only wanted to inspect, and requirements drift silently. The
  harness forbids this: finished steps and conceptual invariants are frozen while
  a plan executes.
- **Accidental mode entry.** If the discussion mode were inferred from the text
  of a prompt, a message whose body merely *resembles* a discussion heading could
  silently enter discussion mode, and identifiers read out of prose could be
  wrong.

TAUQE removes both by carrying the mode and its target as typed request fields
and by giving the discussion turn its own protocol with no write capabilities.

## 2.8.2. A Typed Target, Not a Textual Prefix

The request that opens a focused discussion carries a typed `DiscussionTarget`:

- `Plan { id }` — discuss a plan, addressed by plan identifier.
- `Review { id }` — discuss a review session, addressed by session identifier.

Alongside the target, the request carries `focused_items` (the specific plan or
review items the developer pointed at) and a free-form `comment` (the question or
instruction). The server composes the discussion prompt from these typed fields,
so identifiers travel verbatim and the mode is never guessed from prose. A plain
ask with no target continues the *current* anchor instead of opening a new one
(see 2.8.4).

## 2.8.3. The Structured Discussion Protocol

A discussion turn is always routed through the structured JSON protocol,
regardless of the `develop.protocol` configured for ordinary turns. The reason
is concrete: the discussion system prompt instructs the model to answer with the
discussion JSON schema, so pairing it with an XML stream filter would leak the
raw JSON scaffolding into the REPL. The protocol is therefore derived from the
*kind of the turn*, not from configuration alone.

The discussion response schema is a strict object whose `message` field is
declared first:

- `message` — the conversational answer, formatted as Markdown. Because it is
  declared and required first, the harness streams it to the interface
  immediately while the rest of the object is still arriving.
- `plan_update` — an optional structured plan change (`save`, `update`, or
  `delete`) applied only at the end of the turn.
- `context_requests` — additional files the model asks to inspect.
- `user_language` — the detected natural language of the developer's request.

On top of the schema, the discussion protocol advertises a reduced capability
set. Plan creation and plan update are available; file edits and toolchain
verification are not. The model can reason about and propose plan structure, but
it cannot touch code from inside a discussion.

## 2.8.4. The Focus Anchor and Its Lifecycle

The active focus is represented by `DiscussionFocus`:

- `None` — free discussion, with no anchored entity.
- `Plan { plan_id, focused_items }` — anchored to a plan.
- `Review { review_id, focused_items }` — anchored to a review session.

The server owns the authoritative value; the client mirrors it for rendering and
receives every change through a typed focus-changed event. The lifecycle is:

1. **Set.** An explicit discussion target replaces the anchor. This does not
   reset the REPL session history, so the developer keeps the running dialogue
   while switching the entity under discussion.
2. **Continue.** A plain ask does not clear the anchor. While a focus is active,
   the current state of the focused plan or review is injected into the prompt on
   every such turn, so follow-up questions stay grounded in the live
   architecture without re-selecting it.
3. **Stale anchor.** If the focused entity no longer exists (for example, the
   plan was deleted elsewhere), the server drops the stale anchor and answers the
   raw prompt instead of failing the turn. The focus is a convenience, never a
   precondition for a turn to succeed.
4. **Clear.** Detaching returns the dialogue to free mode. Clearing the
   conversation history also detaches the focus, so a fresh session never
   inherits a dangling anchor.

## 2.8.5. Interface Indication

While a focus is active, the REPL reserves a single status line directly above
the prompt for a focus banner. The banner is a pure indicator: it names the
focused entity with a `[PLAN]` or `[REVIEW]` badge, its identifier and title, how
many items are targeted, and a short hint for releasing the focus. It carries no
command of its own and is carved out of the response area, so it never competes
with the prompt for space.

Showing the anchor is deliberate. The developer should always be able to tell, at
a glance, whether the next message will be answered as free dialogue or grounded
in one specific plan or review.

## 2.8.6. Safe Detach

Detaching the focus is bound to `C-g` in the REPL (the abstract primary command
modifier, `Ctrl` by default) and is guarded by a confirmation dialog whose
default focus is `[ Cancel ]`. The guard exists because detaching is easy to
trigger yet discards the explicit grounding of the conversation; a stray
keypress should not silently drop it. When no focus is active, `C-g` is inert and
the interface simply reports that there is nothing to detach.

`Esc` is deliberately *not* the detach key. In the REPL it keeps its long-standing
meaning — interrupt a running turn, or clear the prompt on a second press — so
the focus reset no longer competes with the most-used key on the keyboard. The
same `C-g` chord is reused in the History view for a different, similarly
destructive action (clearing the conversation history), each gated by its own
confirmation; the two never fire together because each is scoped to its own view.
