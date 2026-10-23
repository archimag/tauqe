# Turn Scratchpad

A bounded turn can span many rounds: a discovery round, a failed staging attempt, a
healing cycle. Between those rounds the model holds no memory of its own — the
prompt is re-sent, not remembered. The scratchpad is the carrier of that working
state.

---

## 1. Motivation & Invariants ("Why")

### 1.1 The problem: state that outlives a round but not a turn

Multi-round work needs continuity. A model that has established which files it
already inspected, which contract it still needs, and which hypothesis a compiler
error just refuted must not start the next round from zero. Re-deriving that state
costs tokens and invites contradictions, because a model that forgets its own
findings reopens the same files and re-tests the same guesses.

### 1.2 Why not simply use conversation history

Session history is durable, user-visible, and expensive. It is the record of the
engineering dialogue, kept across turns and compressed only when it must be. Writing
intermediate scratch reasoning into it would pollute the persistent transcript,
inflate every later prompt, and mix private deliberation with the answer the
developer is reading.

### 1.3 The invariants

1. **Turn-scoped.** The scratchpad exists for the duration of one turn and is
   discarded when that turn ends.
2. **Never persisted.** It is not written to session history, not recorded in a
   commit, and not stored on disk.
3. **Never streamed.** It is removed from the assistant text, so the interface
   never renders it.
4. **No capability.** It moves no bytes and executes nothing; it is memory, not a
   tool.
5. **Bounded.** It is capped so it cannot become a second context window.

---

## 2. Mental Model & Ontology ("What")

The scratchpad is the model's **private notebook for one turn**. Three terms are
enough to describe it:

- **Scratchpad** — the accumulated text the model records for itself. It arrives
  either as a `scratchpad` block or as a self-closing `scratchpad` tag carrying a
  `note` attribute, and both forms contribute to the same buffer.
- **Turn scratchpad block** — the re-injection of that buffer into the next round,
  rendered as one explicit block after the whole stable prefix of the system
  message.
- **Budget** — a hard character cap on the buffer, with truncation on a character
  boundary so multi-byte text is never split.

It is deliberately *not* the plan. A plan is durable, structured, user-visible, and
owned by the developer; the scratchpad is ephemeral, unstructured, and owned by the
turn.

---

## 3. Mechanics & Lifecycle ("How it behaves")

1. **Capture.** When a round is decoded, the scratchpad commands are extracted and
   their content is appended to the turn-local buffer. The same pass removes the
   scratchpad from the conversational text, so the streamed answer and the final
   answer contain neither the tag nor its body.
2. **Accumulate.** The buffer persists across discovery rounds, staging retries, and
   healing rounds. A later round does not need to restate what an earlier round
   recorded.
3. **Re-inject.** Before the next request, the buffer is rendered as the last block
   of the system message — after the context files and the active plan. Keeping it
   last is what preserves the provider's cached prefix: the volatile tail changes on
   every round by design, while everything in front of it stays byte-identical.
4. **Bound.** The buffer is capped at 4,000 characters. Truncation lands on a
   character boundary, and the prompt instructions ask for dense bullets instead of
   prose so the cap is reached rarely.
5. **Discard.** When the turn ends — success, error, blocked, or cancellation — the
   buffer is dropped with the rest of the turn state.

The capability that carries the scratchpad is enabled per profile, so a mode that
does not advertise it cannot produce one, and a scratchpad tag in such a turn is
inert text.

---

## 4. Boundaries & Anti-Patterns ("What it is NOT")

- **It is not persistent memory.** Nothing written there survives the turn, and
  nothing reads it in a later turn.
- **It is not a notes file.** It is never stored, so it cannot be edited, grepped,
  or reviewed.
- **It is not a plan.** Plans are durable artifacts with statuses, dependencies, and
  immutability rules; the scratchpad has none of that.
- **It is not a tool.** It requests no file, runs no command, and grants no
  permission that the active capability set does not already grant.
- **It is not a substitute for reading.** Requesting a contract through discovery is
  still the only way to know its exact definition; a scratchpad note is a reminder,
  never evidence.
