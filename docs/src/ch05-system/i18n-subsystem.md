# Zero-Hardcode Interface Localization (i18n)

The interface speaks English by default, and it does so through a catalog rather than
through literals: every title, tab, button, badge, and notification the terminal
frontend renders is resolved from a stable key. A translation is generated on demand by
a language model and then accepted by deterministic code, because a model can translate
a phrase but cannot measure a terminal grid.

---

## 1. Motivation & Invariants ("Why")

### 1.1 The literal leak

A string written inline in a renderer cannot be translated without editing code. Across
a large interface this produces three failures at once: the same concept acquires several
slightly different spellings, a layout budget is discovered only when a translation
overflows its column, and there is no single place to look for the text a surface shows.

### 1.2 Why the model translates and the code measures

Translation is a generative task. It needs the semantic intent of a string — is this a
destructive button, a tab title, a status badge? — and that intent is exactly what a
surrounding code path no longer expresses once the string is isolated.

Acceptance is not generative. A terminal cell is a physical unit: a Latin letter occupies
one, and a CJK glyph occupies two. A model asked to stay under ten columns produces
something plausible and often wrong, and the failure mode is a broken layout rather than
a bad sentence. The subsystem therefore separates the two concerns absolutely: **the
model proposes text, the code decides whether it fits.**

### 1.3 The invariants

1. **English is the only embedded source of truth.** There is exactly one authoritative
   text per concept, and it lives in the canonical catalog.
2. **Every user-facing string resolves through a key.** A literal in a renderer is an
   architectural defect.
3. **Every entry declares its semantic context** and, when it is bounded, its maximum
   width in terminal columns.
4. **Generative proposal, deterministic acceptance.** Width is measured in cells, never
   in bytes or scalar values.
5. **The fallback is always English.** Missing, blank, over-budget, or corrupt text
   degrades to the canonical English original; the interface never renders a gap.
6. **At most one custom locale exists.** One language is active at a time, stored as one
   file.
7. **The client never contacts a provider.** Synthesis is a daemon operation with the
   same operation slot, cancellation, and usage accounting as any other call.
8. **Interface language and response language are independent planes.** The interface
   locale restyles the frontend; the response language pins the natural language of the
   model's answer.

---

## 2. Mental Model & Ontology ("What")

### 2.1 Terms

- **Canonical catalog** — the embedded list of every interface entry, in English.
- **Catalog item** — one entry: `key`, `english`, `context`, `max_width`.
- **Key** — a stable dot-notated identifier (`modal.confirm`, `tab.repl`,
  `status.done`). A key is never translated and never re-derived from text.
- **Context** — a sentence describing where the string appears, which is what the model
  translates against.
- **Budget** — the maximum display width in terminal columns, or unbounded for help
  text, descriptions, and paragraphs.
- **Custom locale** — the single stored translation set: a language name plus a
  key-to-text map.
- **Sanitization** — the deterministic pass that turns a proposed translation set into an
  accepted one.
- **Active locale snapshot** — the resolved key-to-text map published process-wide, so a
  renderer without an application handle still resolves the same text.

### 2.2 A catalog item

| Field | Meaning |
| --- | --- |
| `key` | stable identifier used for lookup |
| `english` | the canonical text, and the fallback |
| `context` | semantic usage description handed to the translator |
| `max_width` | column budget for a fixed grid cell, or absent for unbounded text |

A bounded entry exists because its surface is a fixed grid cell: a tab title, a button
label, a status badge. An unbounded entry exists because its surface wraps: help text, a
command description, an explanatory paragraph. Marking a paragraph as bounded would force
a telegram style, and marking a button as unbounded would overflow its row.

### 2.3 Two independent language planes

| Plane | Owns | Selected by |
| --- | --- | --- |
| Interface locale | every rendered label of the frontend | the locale dialog (`M-L`) |
| Response language | the natural language of model output | the language directive of a turn |

They are deliberately independent. A developer may read a translated interface while
asking the model to answer in English, or the reverse, and neither choice rewrites the
other.

### 2.4 Storage

The custom locale is one JSON document at `$XDG_DATA_HOME/tauqe/custom_locale.json`,
with `%LOCALAPPDATA%`, `$HOME/.local/share`, and `%USERPROFILE%/.local/share` fallbacks.
It holds the language name and the key-to-text map. Writing is atomic: the payload goes
to a unique temporary sibling named with the process id and a nanosecond timestamp, then
is renamed into place, so a concurrent process or an interrupted write cannot leave a
half-written catalog. Deleting the file is the whole reset operation.

The catalog itself is never stored: it is compiled into the binary, which is what
guarantees that every key resolves even before any locale exists.

### 2.5 The wire contract

`locale/generate` carries the requested language together with the catalog entries to
translate, and answers with the language and a key-to-text map. A missing key is not an
error: the client falls back to English, and the daemon's own sanitization guarantees
that every requested key comes back populated.

Synthesis runs in the **Scribe** role, because translating a fixed list of short strings
is a routine text transformation rather than an engineering dialogue. It is a
structured-output operation, so the model answers inside an enforced JSON schema instead
of free-form text.

---

## 3. Mechanics & Lifecycle ("How it behaves")

### 3.1 Resolution at render time

A renderer asks for a key. The engine looks the key up in the embedded English catalog
and returns the canonical text, unless an active custom locale supplies a translation
that is non-blank *and* fits the entry's budget, in which case the translation wins. A
key that exists nowhere resolves to an empty string rather than a panic, and the test
suite asserts that every registered key resolves to non-empty text both in English and
under an empty custom locale.

Because a renderer deep in the tree may not hold an application handle, the resolved map
is also published as a process-wide active-locale snapshot. Installing a locale
recomputes that snapshot from the engine, so every surface — including the shared modal
footer vocabulary — switches together.

### 3.2 Width validation in terminal cells

Width is measured with Unicode display width, not with length:

- a Latin letter is one cell;
- a Cyrillic letter is one cell;
- a CJK glyph is two cells;
- a combining mark is zero cells.

A translation that occupies more cells than its budget is rejected even when it contains
fewer scalars than the budget, which is exactly the case a byte- or character-based check
gets wrong.

### 3.3 The synthesis pipeline

1. The developer opens the locale dialog and names a target language in free form. An
   empty name is refused locally with a warning, and the dialog stays open for
   correction.
2. Synthesis is refused while a model turn or a review is running, and the daemon refuses
   it while another operation already holds the single operation slot.
3. The request carries the whole canonical catalog: every key, its English text, its
   semantic context, and its budget.
4. The daemon resolves the Scribe model, builds a system prompt that states the
   operational invariants (translate meaning, keep identifiers verbatim, respect the
   column budget, emit exactly one translation per key, answer in JSON only) and a user
   prompt that lists the entries, and requests a strict schema named
   `locale_translation`.
5. The streamed answer is parsed tolerantly: code fences and wrapper prose are stripped,
   entries with a blank key are dropped, and a later entry overrides an earlier one for
   the same key. An answer with nothing recoverable yields an empty map rather than an
   error.
6. The map is sanitized deterministically against the requested catalog, and the result
   is returned.

### 3.4 The deterministic sanitization ladder

For every requested entry, in order:

1. **It fits.** The translation occupies no more cells than its budget (or the entry is
   unbounded), so it is accepted verbatim.
2. **It is shortened.** The translation exceeds its budget, so it is cut to the budget
   minus one cell and an ellipsis (`…`) is appended — but only while at least half of the
   original width survives the cut. A word that keeps most of its shape still reads; a
   two-letter stump of a long phrase does not.
3. **It reverts to English.** Otherwise — or when the translation is missing or blank —
   the canonical English text is used, itself truncated to the budget when necessary.

Keys the model invented are ignored, so the result never carries an orphaned entry, and
keys the model omitted are filled from English, so the result never carries a hole.

### 3.5 Failure and cancellation

Every terminal path is named and accounted. A missing provider key answers with a
credentials error *before* the operation slot is taken, so a failed precondition never
leaks a dangling slot. A competing operation is refused, a provider failure surfaces as a
model error, and cancellation surfaces as a cancelled result.

Cancellation matters because synthesis is the slowest dialog in the interface. Pressing
the abort chord while the request is in flight dismisses the dialog and cancels the call,
and the hard cancel is sent only when no unrelated turn or review is running, so an
accidental abort never kills unrelated work. Whatever the call already spent is folded
into the usage ledger exactly once on the way out, so a cancelled translation is charged
for its partial cost instead of vanishing from the totals.

### 3.6 Hot swap and reset

An accepted result is applied without a restart: the engine adopts the new custom locale
and the active-locale snapshot is recomputed, so the next rendered frame is already
translated. Resetting to English deletes the single stored file and revokes the custom
locale in the engine. If the file cannot be deleted, the interface still reverts and
reports that the stored copy survived, so the developer learns the choice will return
after a restart.

### 3.7 The response-language directive

The other language plane travels with a turn. When the model emits the language
capability — as an attributed tag or as a short body — the harness extracts the language
name, removes the tag from the conversational text so it never reaches the reader, and
pins the named language as the response-language directive of the turn. Every later round
of that turn, and every later answer of the session, is then formulated in that language.

The two planes meet only in the mind of the reader: a translated interface can render a
translated dialog while the model answers in whatever language the directive pins.

---

## 4. Boundaries & Anti-Patterns ("What it is NOT")

- **It is not a shipped translation.** No translated catalog is committed; the only
  committed text is English.
- **It is not `gettext`.** There is no `libc` locale lookup, no message-domain catalog on
  disk, and no environment-driven language selection.
- **It is not a second source of truth.** A translation is an overlay; the English catalog
  remains authoritative.
- **It is not model-verified geometry.** The model never decides whether its own text
  fits.
- **It is not a model-driven redesign.** The model supplies text for known keys; it never
  introduces a surface, a key, or a layout.
- **It is not the answer language.** The interface locale and the response language are
  separate planes, and neither rewrites the other.
- **It is not client-side provider access.** The frontend sends a typed request; the
  daemon owns the call.
- **It is not a silent failure surface.** Unknown keys resolve to empty rather than
  panicking, and the suite asserts that every registered key resolves to non-empty text.

---

## 5. Related Chapters

- [Keyboard-First Interface](tui-ux.md) — the surfaces whose text is localized, and the
  chord that opens the locale dialog.
- [Configuration and Project Preparation](configuration.md) — where the workstation file
  lives and what it declares.
- [Model Roles and the Coder Ladder](../ch01-foundations/model-roles.md) — the Scribe role
  that performs synthesis.
- [The Autonomous Bounded Turn FSM](../ch01-foundations/bounded-turn-fsm.md) — the
  capability that carries the response-language directive.
- [Structured Diagnostics and Verification Healing](../ch03-project/verification-healing.md) —
  the structured-output contour this operation follows.
