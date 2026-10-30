# Keyboard-First Interface

TAUQE's terminal interface is not a chat window with bubbles. It is a structured,
live document of the state of the work, operated entirely from the keyboard. Two
design lineages meet here: **Magit** supplies the high-density state document with
collapsible sections and contextual cursor operations, and **SLIME/Swank** supplies
the split between a thin interactive frontend and a daemon that owns the domain.

The authoritative key matrix is part of this chapter on purpose: a binding documented
somewhere other than the code would drift from the binding the application actually
intercepts.

---

## 1. Motivation & Invariants ("Why")

### 1.1 State cannot be rendered as a conversation

A bounded turn has phases, a reasoning transcript, a set of proposed file diffs, a
verification result, and a plan stepper. A conversation transcript can show one linear
text stream; it cannot fold a diff, keep a tree node under a cursor, or mark which file
the compiler rejected. The unit of the interface is therefore a **document of state**:
hierarchical, foldable, and dense.

### 1.2 Why the client is thin

The domain — Git, toolchain verification, provider calls, plan storage — belongs to the
daemon. The frontend owns layout, keyboard, and local editor state, and speaks typed
RPC. That split is what lets a second frontend reuse the same engine without
re-implementing a transaction, and it is why the interface never runs `git` and never
contacts a provider.

### 1.3 The invariants

1. **One binding, one declaration.** Every chord is declared once in a typed command
   map that both the dispatcher and the Help window read.
2. **Typing always types.** While a text editor owns the keyboard, every printable
   character is inserted and bare command letters are unconditionally blocked.
3. **Case encodes scale.** A lowercase letter acts on the selection; its uppercase
   counterpart acts on every applicable item.
4. **`x` / `X` mean execution only.** A key pressed while disproving a step never
   destroys it.
5. **No function keys.** Every action is reachable through a bare key, a command
   modifier, or a dedicated navigation key.
6. **Destructive and long-running actions ask first**, with the safe choice focused.
7. **Every color is a semantic role**, never a raw ANSI name.
8. **Every user-facing string is a catalog key**, never a literal.
9. **The terminal is restored on every exit path**, including panics and signals.
10. **Auxiliary views never print shortcut banners.** The Help dialog is the single
    discovery surface.
11. **A patch is never blind.** Every staged file keeps one summary row — its
    operation, path, patch count, and verification mark — and the whole change
    set opens as one isolated two-pane card on the inspector chord.

---

## 2. Mental Model & Ontology ("What")

### 2.1 Views

Five primary views form the workspace, and History is always the last tab:

| Chord | View | Role |
| --- | --- | --- |
| `Ctrl+1` | REPL (Develop) | the execution canvas: turns, streaming answer, diffs, plan stepper |
| `Ctrl+2` | Plans | concept tree and implementation steps |
| `Ctrl+3` | Review | audit sessions and findings |
| `Ctrl+4` | Context | pinned, user, and auto layers with token metrics |
| `Ctrl+5` | History | the chronological session journal |

The digits are reachable on the primary modifier (`Ctrl+1` … `Ctrl+5`) and on the meta
modifier (`M-1` … `M-5`), so a terminal without a dedicated Meta key still reaches
every view.

Ancillary surfaces are reached without leaving a view: model selection on `C-M` /
`M-M`, the theme selector on `M-T`, the interface-language dialog on `M-L`, the
usage breakdown on `M-U`, and the diff inspector on `C-V` / `M-V`.

**Model selection is one component, not five.** The picker on `C-M` / `M-M` and the
four dialogs that choose a model for a role — the review launcher, the plan refinement
form, the finding decomposition form, and the point-execution form — all draw their rows
through the same selector. One list state, one cyclic navigation, one positional
quick-select, one scroll window, and one click-to-index translation serve every surface,
so a picker row and a dialog row can never disagree about which rung `Enter` dispatches
or which model a click selects. The picker differs only in the role it presents
(`primary`) and in its card chrome.

**The picker presents the configured ladder exclusively.** It lists exactly the tiers
declared under `[models.primary]`, each shown as `model [effort]`, and it offers no path
into the provider's full catalog. A model that is not part of the configuration is added
to `tauqe.toml`, never improvised inside a turn: turning a configuration decision into an
incidental keystroke would let one picker visit bypass the ladder the harness is built
around. Activating a row sends the whole tier — the model together with its reasoning
depth — and reopening the picker highlights the rung the developer picked, matched by both
coordinates with a fallback to the first entry of the same model. When the role assignment
has not reached the client yet the picker stays closed and says why, because an empty
primary list is a missing configuration rather than an invitation to guess (see
*Configuration and Project Preparation*).

### 2.2 The turn as a document

While a turn runs, the REPL renders it as a live document rather than a growing
paragraph:

- **Lifecycle badges** report the turn status (awaiting, thinking, responding, editing,
  verifying, discovery, staging, healing, done, cancelled, error) and the current phase,
  with a round counter where the phase repeats.
- **The reasoning transcript** streams as a bounded sliding tail: the most recent lines
  with a running line count, rendered with lightweight per-line styling. The full
  Markdown parse and syntax highlighting run once, when reasoning finalizes. The raw
  transcript is held in a rolling window with a hard cap (128 KiB); the cut lands on a
  Unicode boundary and the line counter stays monotonic, so a long turn never appears
  to shrink.
- **The proposed change set** is a compact list: one summary row per staged file,
  carrying its operation badge, path, patch count, and verification mark, with the
  selected file under a cursor. The canvas never inlines a patch body, so an answer
  and its diffs read as one dense document instead of an interleaved scroll; the
  full patch is one chord away in the inspector. The list belongs to the *current*
  step: when a batch advances, the previous step's files are already committed and
  leave the working area.
- **The plan stepper** appears only when the turn carries plan-step metadata: a title
  and one title-only row per step in scope. It never expands diffs or concept nodes,
  and an ad-hoc turn renders without it.

### 2.3 Focus: editor versus viewport

The Develop view has exactly two focus targets, because arrows and `Ctrl+P` / `Ctrl+N`
mean different things in a text buffer and in a diff list.

- **Editor focus (default).** Every printable character is inserted; Emacs and Readline
  chords move the cursor, kill, and yank; prompt history cycles with the configured
  meta chords.
- **Viewport focus.** `Ctrl+N` / `Ctrl+P` and the arrows walk the compact list of
  staged files, one summary row per file, and the viewport follows the selection;
  once the selection sits at the edge of the list the same chord scrolls the
  response instead. The explicit `Ctrl+[` / `Ctrl+]` chords switch files directly
  and `Ctrl+L` recenters on the selected row. While the viewport holds focus, the
  prompt border is dimmed and titled `[VIEWPORT]`.
- **The diff inspector.** `Ctrl+V` / `Alt+V` — the symmetric command chord — opens a
  fullscreen audit of the change set from the prompt *and* from the Viewport, so
  reaching it never requires the focus switch first. In the Viewport, `Enter` on the
  selected file and the bare `v` chord open the same card. The audited files sit on
  the left with their operation and verification badges, and the isolated diff of
  the highlighted file on the right with a line-number gutter and its own scroll
  offset. `Tab` and `Shift+Tab` switch the active pane, whose border carries the
  accent; `↑` / `↓` and `n` / `p` then move the file highlight in the list or scroll
  the patch in the read area, `j` / `k` scroll the patch from either pane, `[` / `]`
  switch files at any time, `PgUp` / `PgDn` page the patch, and `Esc` or `q` closes
  the card and returns to the reading position it was opened from. The card reads the
  live staged set: a file that arrives mid-audit appears, its patch count and status
  update in place, and the file the stream currently writes carries the animated
  spinner beside its `[RUN]` badge. The inspector never mutates the staged set and
  never starts a turn.
- **One body, one place.** The canvas draws no patch line at all: a patch is read in
  the inspector, and the Viewport keeps the answer, the reasoning transcript, and the
  compact file list intact.
- **Switching.** `Ctrl+Space` or `Shift+Tab` toggles focus and `Esc` returns to the
  prompt. Any key that types or edits text returns focus to the prompt and is applied
  there, so no keystroke is lost.

### 2.4 The command vocabulary

- **`CommandSpec`** — one declaration: an abstract chord, a typed action, a cognitive
  category, and a localization key for its help text.
- **`KeyChord`** — the abstract chord alphabet: `Primary` (`C-`), `Meta` (`M-`),
  `PrimaryOrMeta`, `Shift`, `Bare`, `Ctrl`, and `Alt`. `C-` and `M-` resolve through
  the configured modifiers, so no view hardcodes a physical modifier; raw `Ctrl` and
  `Alt` are reserved for the chords that must never be remapped (the cancel chord and
  `Ctrl+G`).
- **`CommandCategory`** — `Essential` (costly: sends, executions, squashes),
  `Interface` (navigation and view state), and `Helpers` (copy, filter, recenter). The
  Help dialog renders them in that canonical order and omits an empty tier.
- **Command maps** — one static map per scope: global, the shared text editor, each
  view, the Develop Viewport, the diff inspector, and the shared dialog map. A
  collision validator in the test suite enumerates printable characters and
  navigation keys across the modifier sets and asserts that no single event
  resolves to two actions inside one map, so declaration order never decides what
  a key does.

### 2.5 The modal card

Every dialog is one card with a fixed anatomy, and no dialog may trade a spacer for
content height:

- **Header** — a border title naming the operation and its status.
- **Top inset** — one empty row, so the body never collides with the title.
- **Body** — the functional area: an alert, a scrollable list, a form, or a diff.
  Logical blocks inside the body are separated by one empty row.
- **Pre-action spacer** — one empty row between the body and the controls.
- **Action bar** — explicit buttons (`Confirm` / `Cancel`, a picker's actions, the
  three-way stop).
- **Docked help panel** — a non-interactive, filled strip docked to the bottom inner
  row, carrying the chords the dialog actually intercepts.
- **Horizontal padding** — one column of inner margin on each side, shared by the body,
  the bar, and the hint, so text is framed rather than clipped by a border.

The geometry helpers that report clickable rectangles derive their coordinates from the
same layout formula the render path uses, so a shifted action bar can never
desynchronize a click from the button it appears to hit.

### 2.6 The semantic design system

A view never names a color. It asks for a semantic role — `canvas`, `surface`,
`overlay`, `code_bg`, `border`, `text_primary`, `text_secondary`, `text_muted`,
`text_inverse`, `button_bg`, `button_active_bg`, `status_info`, `status_success`,
`status_warning`, `status_error`, `diff_added`, `diff_removed`, `diff_context` — and
the engine decides the value. Layering is expressed through the surface roles, so a
modal occludes what is behind it at every color depth.

Presets are authored once in 24-bit sRGB and projected onto the depth the terminal
supports: exact RGB under truecolor, the nearest entry of the fixed 256-color cube above
index 16 under `ansi256` (the remappable low slots are never emitted, because
Solarized-class schemes reassign them and a border can disappear into its background),
and attribute-based reversed bands under `ansi16`. Contrast is enforced by automated
tests rather than by eye.

### 2.7 The Markdown surface

Assistant answers, reasoning, plan details, and history entries render through one
pipeline, so a document reads consistently in every view. Column widths are derived
from the widest cell and fitted to the available width; cells wrap on word boundaries
and split only a word wider than its column, always on a Unicode scalar boundary, so
multibyte text is never cut. Column alignment is honored, vertical borders stay on one
terminal column across wrapped physical lines, and one horizontal separator is drawn
between consecutive data rows. Fenced code blocks render borderless with a background
band, a language label, and a copy control, highlighted per language through `syntect`
against the active theme. Reasoning output uses a distinct dimmed palette, so private
deliberation is visually separate from the answer.

### 2.8 Telemetry surfaces

The footer carries three compact values of the accumulated session — money when the
provider reports it, total tokens otherwise, and the harness-measured session time — and
drops a whole field rather than truncating a number. The full breakdown (prompt,
completion, reasoning, and cached tokens, cost, time, completed operations, and the
persisted project lifetime) lives in a read-only dialog on `M-U`, which spends no tokens
and never displaces a pending confirmation.

### 2.9 The chronicle: in-flight and rolled-back records

The History view is a chronicle of transactions, not a transcript. Each row is one
record — user, assistant, or system — and its lifecycle status is rendered as a badge
beside the kind:

- **`[IN-FLIGHT]`** marks a prompt whose turn is still running. The record enters the
  view the moment the prompt is accepted, so switching tabs mid-turn never hides what
  the harness is doing.
- **`[ROLLED BACK]`** marks a prompt whose turn failed or was cancelled. The row keeps
  the reason as its summary and gains a restore hint line, so a vanished conversation
  step is explained rather than merely missing.

On this side a rollback is neither an append nor a deletion: the event carries the
authoritative record, and the view replaces that one row in place. A record the view
has never seen is inserted, so a failure that lands before the prompt was rendered
still leaves a trace. Compaction follows the same discipline — the summarized head is
dropped, the summary takes the first row, and the preserved tail keeps its order.

Three stores back this view and the REPL, and they stay separate: the session journal
(`.tauqe/history.jsonl`) is the model's dialogue memory, the History view adds the
running and the discarded records, and the prompt recall list holds only what the
developer typed. The recall list lives in the per-user state directory —
`$XDG_STATE_HOME/tauqe/prompts.history`, falling back to
`$HOME/.local/state/tauqe/prompts.history` — and never inside a checkout. A prompt may
embed a key, an internal hostname, or a fragment of production data, and file
permissions do not protect a file from version control, so the placement itself is the
guarantee: a working tree cannot stage what it never receives. The file is written
atomically with owner-only permissions and is capped, and when the environment declares
no usable state directory the session keeps its prompts in memory only.

---

## 3. Mechanics & Lifecycle ("How it behaves")

### 3.1 The navigation minimum

Every structural view implements the same symmetric movement set:

| Motion | Keys |
| --- | --- |
| Next / previous row | `n` / `p`, `↑` / `↓`, `k` / `j` |
| First row | `Home`, `M-<`, `Ctrl+Home` |
| Last row | `End`, `M->`, `Ctrl+End` |
| Paged scrolling | `PgUp` / `PgDn` |
| Accordion walk | `Ctrl+N` / `Ctrl+P` |
| Recenter the active item | `Ctrl+L` |
| Refresh state from the daemon | `g` |

The redundancy is deliberate: `M-<` and `M->` mirror Emacs, `Home` and `End` remain
universal fallbacks for terminals that swallow `Alt`, and `Ctrl+Home` / `Ctrl+End`
cover multiplexers that intercept both. `g` is the Magit/Dired refresh: it re-reads
state over RPC without starting a turn, so it is cheap and needs no confirmation.

### 3.2 The verb matrix

The verbs are fixed across Plans, Review, Context, History, and the REPL viewport; only
the data underneath changes.

| Intent | Selection (lowercase) | Batch (uppercase) |
| --- | --- | --- |
| Execute | `x` — run the selected step or finding | `X` — run every ready step or open finding |
| Architect | `r` — decompose the selected item | `R` — decompose every item awaiting it |
| Copy | `c` / `y` — copy the selected item | — |
| Clear a collection | — | `Shift+C` |
| Remove one entity | `Delete`, with confirmation | — |
| Fold | `Tab` / `Space` | `a` — fold or unfold everything |
| Status | `t` (or `s`) opens the status dialog | — |
| Refresh | `g` | — |

Reserved meanings follow from the same scale rule. `r` / `R` belong to the Architect, so
refresh uses `g`. `N` creates a new review session, because creation and decomposition
are different acts and must not share a letter. `Shift+A` in Review runs the Architect
post-analysis over the selected session, so the bare `a` keeps the global fold. `c`
and `y` only place content in the clipboard, while a mass clear requires `Shift+C` and
a confirmation whose default focus is `Cancel`. A view with nothing to clear leaves
`Shift+C` unbound rather than borrowing a destructive chord for a copy.

### 3.3 Dialog interaction

- `Tab` and `Shift+Tab` cycle focus, `←` / `→` step between adjacent buttons, and
  `Enter` or `Space` activates the focused control.
- `Ctrl+N` and `Ctrl+P` are the vertical field walk: they move to the next and the
  previous input field, and inside a field that owns several rows — the review
  launcher's Target Paths — they walk those rows and leave the section only at its
  boundary. The vertical axis therefore never depends on `Tab`, and a multiline
  editor keeps the same chords for cursor line movement.
- `Tab` is the contextual completion chord: while a completion menu of directory
  prefixes is open it accepts the highlighted candidate, and without a candidate it
  falls through to the shared focus move.
- A multi-row path field grows with `Ctrl+J`, which opens the next row only while the
  active row already carries a usable pattern, and shrinks with `Backspace` on an
  empty row, which returns the cursor to the previous row.
- `Esc` dismisses a dialog and takes its cancel path. `Ctrl+G` is normalized to `Esc`
  once, before any handler runs, so the Emacs `keyboard-quit` chord behaves identically
  everywhere without a per-handler binding and without a `SIGINT` handler.
- Confirmation dialogs accept `y` / `Y` to confirm immediately and `n` / `N`, `q`, or
  `Esc` to cancel.
- Status is assigned intentionally through a status dialog (Plans: discussion, todo, in
  progress, done, blocked, cancelled; Review: discussion, todo, in progress, fixed,
  rejected), never by an accidental cycle.

### 3.4 The three-way batch stop

Cancelling a running batch is a decision, not a dismissal. The modal offers three
explicit actions:

- **Continue**, holding the safe default focus, keeps the queue running, so an
  accidental keypress never discards committed work;
- **Soft Stop** lets the running member finish its turn, verification, and commit, then
  halts the queue before the next member;
- **Hard Stop** aborts the running member immediately.

Direct accelerators `c`, `s`, and `h` choose an action without moving focus. While a
soft stop is pending the footer says so (`[Stopping after current step]`), so a graceful
drain is never confused with an immediate abort. Hard cancellation wins when both
signals are pending.

### 3.5 Confirmation before cost

Anything destructive or long-running asks first, with the safe choice focused: quit
with a non-empty draft, undo of an AI commit, squash, cancel generation, delete a plan
or a review session, clear a context layer, clear history, and clear a plan horizon.
Undo verifies that an undoable commit exists before prompting and reports a notice
otherwise, so the dialog never opens onto an impossible action. Clearing a horizon that
has already started escalates to a strict two-tier confirmation warning that the plan
will diverge from committed repository history.

### 3.6 Progress and notifications

A single interactive turn notifies as soon as it finishes. A batch does not: one launch
runs many steps and the developer waits for the whole queue, so intermediate steps raise
no banner and the supervisor emits exactly one typed completion event carrying the final
status (`Completed`, `Failed`, `Blocked`, `Cancelled`, or `Stopped`), the executed versus
scheduled counts, and the elapsed time. Delivery is gated by the workstation file
(`sound`, `desktop`, `desktop_protocol`, `only_unfocused`, `min_duration_seconds`, and an
optional `command` hook), so batching changes *when* the signal fires, never *how* it is
delivered.

### 3.7 Terminal restoration

The interface borrows the terminal: alternate screen, raw mode, mouse capture, and
possibly pushed keyboard enhancement flags. A single restoration routine undoes them in
a fixed order, and it is triggered from three places — the terminal guard on an ordinary
quit, the panic hook, and the termination handler. Because more than one trigger can
fire for one run, the routine is idempotent, and the keyboard enhancement state is
tracked by an atomic flag the first restore clears. A signal restores the terminal and
exits; it is never interpreted as an interface command.

### 3.8 Presentation text comes from the catalog

Every title, label, button, badge, and notification resolves through a catalog key, and
Help descriptions are localization keys stored in the command specifications. Nothing
user-facing is a literal, so switching the interface language restyles every surface
without editing a renderer. The subsystem that owns the catalog, the synthesis, and the
width validation is described in *Zero-Hardcode Interface Localization*.

### 3.9 Transaction feedback: the running prompt and the discarded one

Feedback about a turn's transaction state is delivered at two scopes.

**In the chronicle.** The History view mirrors every lifecycle event the daemon
publishes: an append inserts a record with its status, a rollback replaces the affected
row, and a compaction restructures the head. Nothing is inferred from surrounding
events, and the cached rendering of a row is invalidated when its status changes, so a
badge and its restore hint appear without a reload.

**In the prompt.** When a turn is rolled back, the REPL raises one warning notification
and leaves a persistent hint row in the Develop view until the next turn starts. The
notification states the operative fact — the prompt was not saved to the model history
— and names the recall chord, projected through the configured meta modifier so it
advertises the key the developer actually has. Recall stays reachable by two routes:
`↑` on the first line of the prompt falls through to the same history walk, and the meta
history chord performs it explicitly.

The editor history matters most exactly here. After a rollback the journal no longer
holds the prompt at all, so the recall list is the only copy the developer still has.

### 3.10 Reading a patch

One surface carries a patch: the two-pane inspector. The Viewport answers the
adjacent question — which files the turn staged and whether they verified — in one
row per file, and neither surface is allowed to move anything else.

**The canvas summarizes, the card reads.** A staged file occupies exactly one
Viewport row: the operation badge, the path, the patch count, and the verification
mark. No patch line is ever inlined into the conversation, so a long diff can
neither bury the answer above it nor interrupt the reading of it.

**The walk stops at the list boundaries.** Inside the Viewport one keystroke moves
the file highlight one row; at the first row and past the last one the same chord
scrolls the response instead, so the selection never leaves the staged set.
`Ctrl+L` recenters on the summary row of the selected file through the shared
center — top — bottom cycle, and manual recentering disables auto-scroll.

**The inspector reads the live set.** Opening the card captures the staged list as it
stands, and every edit-stream event refreshes it in place: a file that arrives
mid-audit appears, its patch count and verification badge update, and the file the
stream currently writes carries the animated spinner beside its `[RUN]` badge. The
reading position never jumps — the list and the patch own separate offsets, and an
offset left over from a previously highlighted file is clamped to the body that is
actually drawn.

**The inspector isolates one diff.** Scrolling the patch keeps the highlight in
place, and moving the highlight resets the patch to its beginning, so a shorter diff
never inherits a stale offset. The action row reports the position of the file inside
the audited set and the hunk under the top edge of the pane. Closing the card returns
to the scroll offset and the selected file the audit was opened from, because the
audit never touched them.

**Failure states are named, not guessed.** An empty change set and a file without a
rendered diff each carry their own placeholder inside the pane, and the operation and
verification badges are reserved before the path, so a deep repository path truncates
instead of hiding the status of the file.

**Every label resolves through the catalog.** The operation badge, the titles of both
panes, the progress counters, the placeholders, the docked hint, and the command
descriptions behind `v`, `Enter`, and `Ctrl+V` are catalog entries, so a hot-swapped
locale reaches the compact list and the inspector without editing a renderer.

---

## 4. Boundaries & Anti-Patterns ("What it is NOT")

- **It is not a chat application.** The unit is a foldable document of state, not a
  stream of bubbles.
- **It is not a shortcut wall.** Primary views never render hint bars; the Help dialog
  is the discovery surface.
- **It is not mouse-first.** The pointer mirrors the keyboard through one shared
  geometry, and every action stays reachable without it.
- **It is not a modal trap.** In editor focus, printable characters always type.
- **It is not color-literal.** Raw ANSI names and unstyled modal backgrounds are
  defects, not style choices.
- **It is not a per-view dialect.** Five views share one verb matrix and one navigation
  minimum.
- **It is not a model catalog browser.** It lists the tiers configured under
  `[models.primary]`; a model outside that ladder belongs in `tauqe.toml`, not in a
  picker opened mid-turn.
- **It is not a function-key interface.** No `F1`..`F12` binding exists, and none may be
  added.
- **It is not a domain client.** It never runs Git, never calls a provider, and never
  writes a working-tree file directly.
- **It is not where progress is invented.** Batch counts and statuses come from the
  daemon's typed events, never from optimistic local arithmetic.
- **It is not one history.** The chronicle the developer reads, the journal the model
  reads, and the prompt recall list are three distinct stores: a rolled-back prompt
  stays visible in the first and is deleted from the second.
- **It is not silent about failure.** A rolled-back turn keeps its record, its reason,
  and a restore hint instead of disappearing from the view.
- **It is not a blind diff.** Every staged file keeps one summary row in the
  Viewport, and the whole change set is one chord away in an isolated two-pane
  card; a patch is never inlined into the conversation.
- **It is not an editing surface.** The inspector reads the staged change set. It
  never mutates it, never opens a turn, and never moves the REPL reading position.

---

## 5. Related Chapters

- [The Context Model](../ch02-perception/context-model.md) — the layers the
  Context view renders.
- [Local Plans](../ch04-workflows/plans.md) — the Plans commands and the step stepper.
- [Structured Code Review](../ch04-workflows/review.md) — the Review commands.
- [Discussion Focus](../ch02-perception/discussion-focus.md) — the focus banner and the
  guarded detach.
- [Zero-Hardcode Interface Localization](i18n-subsystem.md) — the catalog every string
  resolves through.
- [Configuration and Project Preparation](configuration.md) — `[input]`, `[theme]`, and
  `[notifications]`.
- [The Autonomous Bounded Turn FSM](../ch01-foundations/bounded-turn-fsm.md) — the phases
  the lifecycle badges report.
- [Git Safety & Transactions](../ch03-project/git-transactions.md) — the commits the undo
  and squash dialogs act on.
