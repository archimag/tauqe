# Discovery Engine

Discovery is the phase that converts "the task depends on a contract I have not
read" into verified knowledge. It widens the working set through typed requests,
runs each request deterministically over Git-tracked files, and keeps every finding
available for the rest of the turn. It is bounded by rounds, budgets, and request
caps, so exploration always terminates.

---

## 1. Motivation & Invariants ("Why")

### 1.1 Unknown unknowns

A task arrives with an incomplete map. The model can see contours, but it cannot
know which module defines a trait, which implementation is relevant, or which
chapter states an invariant. Guessing produces plausible patches against invented
contracts. Discovery replaces guesses with requests: open the file, search the
tracked tree, outline the subsystem, list the directory, load the documentation
chapter.

### 1.2 Why the developer does not curate by hand

Manual context logistics is a failure mode, not a workflow. The developer states
the task; the model asks for what it lacks; the harness decides whether the request
is inside the repository, inside the budget, and inside the round cap.

### 1.3 The invariants

1. **Tracked files only.** Discovery never reads outside the repository and never
   invents a path; an unknown path becomes explicit "not found" feedback.
2. **Every round is bounded.** Rounds, searches, outlines, listings, and the files
   token budget all have configured ceilings.
3. **Deterministic execution.** Searches run in Rust over tracked files; outlines
   come from Tree-sitter; documentation comes from embedded chapters.
4. **Nothing learned is lost.** Findings that are not context files accumulate in a
   turn-scoped journal that survives every later phase.
5. **The cached prefix does not move.** Round findings extend the user message;
   they never rewrite the stable block of the system prompt.
6. **A breach halts with a diagnostic.** No budget silently drops a requested file.
7. **The phase is deterministic; the content is generative.** The model chooses
   what to ask for, never which stage runs next.

---

## 2. Mental Model & Ontology ("What")

### 2.1 A round

A discovery round is one model interaction that may carry any number of typed
discovery commands. The dispatcher resolves repository facts once, hands them to
every handler that owns a command, and folds the results into one outcome.

### 2.2 Commands and handlers

| Command | Handler | What it returns |
| --- | --- | --- |
| `<context_request path access>` | context request | files added to the Auto layer, or "not found" |
| `<repo_search path regex>` | repository search | matching tracked files with hit counts |
| `<repo_dirs />` | directory tree | the compact folder outline |
| `<repo_map path>` | targeted outline | the symbol map of one subtree |
| `<dir_list path recursive>` | directory listing | tracked files of one directory |
| `<doc_request topic>` | documentation | the embedded chapter of one topic |
| `<doc_request topic>` | documentation | the embedded chapter of one topic, in the interactive dialogue only |
Each handler owns its validation, its disk work, and its feedback text. The turn
loop dispatches polymorphically instead of branching on individual command types,
so a capability cannot drift from its validation or its explanation.

### 2.3 Feedback blocks

Findings return as delimited blocks:

- `<context_feedback>` — missing files, an explanation when requested files are
  already loaded, and a notice when the repository could not be inspected.
- `<search_feedback path="..." regex="...">` — per-file hit counts, an omission
  count, or a syntax diagnostic.
- `<repo_dirs_feedback>` — the compact folder outline.
- `<repomap_feedback path="...">` — a targeted outline.
- `<dir_list_feedback path="..." recursive="...">` — a directory listing.
- `<system_documentation>` — authoritative chapters.

### 2.4 The journal

Non-file findings cannot enter the context block: that block is the authoritative
set of repository files, and mixing outlines or documentation into it would pollute
patching. They accumulate instead in the turn-scoped journal, an append-only,
de-duplicated tail of the user message.

### 2.5 Budgets

| Bound | Setting or constant | Default |
| --- | --- | --- |
| Discovery rounds per turn | `context.max_discovery_rounds` | 5 |
| Aggregate context tokens | `context.files_token_budget` | 300,000 |
| Outline tokens | `context.repomap_token_budget` | 4,000 |
| Searches per round | `MAX_SEARCH_REQUESTS_PER_ROUND` | 5 |
| Matching files reported | `MAX_MATCHED_FILES_PER_SEARCH` | 50 |
| Searchable file size | `MAX_SEARCHABLE_FILE_SIZE` | 2 MiB |
| Targeted outlines per round | `MAX_REPO_MAP_REQUESTS_PER_ROUND` | 3 |
| Directory listing entries | `MAX_DIR_LIST_ENTRIES` | 100 |

---

## 3. Mechanics & Lifecycle ("How it behaves")

### 3.1 The round

1. The model emits discovery commands, with or without surrounding text.
2. The dispatcher resolves the tracked file list once, off the async executor
   thread, and only when a command needs it. A failed resolution ends the round
   with an inspection-failure feedback block.
3. Every registered handler that owns at least one command executes in dispatch
   order and folds its findings into one outcome.
4. A files token breach stops the dispatcher immediately: later handlers are not
   run against a context that was never widened.
5. The outcome reports whether the round made progress, and the turn either asks
   again or proceeds to proposal while the round budget lasts.

### 3.2 Opening files into context

For each requested path the dispatcher normalizes the path and requires it to
exist among tracked files or already in context. A path that fails becomes a
missing-file diagnostic. A path whose current access already satisfies the request
becomes an "already satisfied" note instead of a duplicate addition.

Candidate additions are priced as their estimated content plus the wrapper each
`<file>` element renders and compared with the remaining files budget; the number of
files a round admits has no separate ceiling. A breach rejects the whole addition set
and reports the current, requested, and allowed amounts. Additions land in the Auto
layer, upgrade access when needed, and never duplicate a pinned file.

The already-satisfied note is deliberately aggregate-aware: it is emitted only when
the round added no files, ran no searches, and the caller allows the explanation.
A plain developer turn does not need to be told about files it already asked for;
the architect discovery loop does, because a re-request is a legitimate signal that
the model is still reasoning over what it holds.

### 3.3 Searching the tracked tree

A search is a Rust regular expression over tracked files, never a disk scan:

- Service directories (`.git`, `.tauqe`, `target`, `node_modules`) are excluded.
- Binary files, invalid UTF-8, and files above 2 MiB are skipped.
- A symlink escaping the repository is skipped.
- The requested path is a component scope: the root, one directory, or one file.
- Absolute paths, `..` components, and NUL bytes are rejected.
- At most five searches are executed per round; further queries return a limit
  diagnostic.
- At most fifty matching files are reported; the remainder is counted.
- Results are sorted by hit count and then by path, so the same query yields the
  same block.

The regex dialect is RE2-compatible with compiled size and DFA limits. Lookaround
and backreferences are unsupported, and a rejected pattern returns a diagnostic
that names the limitation instead of an opaque engine error.

### 3.4 Navigating structure

- `<repo_dirs />` renders the tracked folder tree and marks pinned files. It is the
  navigation view when the eager symbol map is disabled.
- `<repo_map path="..." />` renders one subtree's symbols within the outline
  budget. The target is matched on whole path components; an unknown prefix yields
  an explanatory block; at most three distinct targets are honored per round.
- `<dir_list path="..." />` lists tracked files. Without `recursive` it reports the
  direct files and the names of direct subdirectories with a trailing slash; with
  `recursive="true"` it reports every tracked file under the prefix. A listing is
  capped at 100 entries, and the block states how many entries were omitted.

### 3.5 Requesting documentation

`<doc_request topic="..." />` returns embedded chapters for `overview`, `context`,
`workflow`, `git`, `interface`, `review`, `plan`, `config`, or `all`. The model
asks for the authoritative chapter instead of reconstructing behavior from partial
source memory.
The capability belongs to the interactive dialogue, where the developer
prepares a project and chooses a workflow. An isolated step execution carries
no documentation request at all: its turn implements one approved architectural
decision, so the Architect already settled *what* must be built and the step
needs the repository rather than the reference manual. The isolation follows
from the capability surface itself, so a turn without the request renders no
documentation rule, declares no documentation field in its schema, and decodes
no documentation tag; the prompt never advertises a channel the decoder would
reject.
### 3.6 The turn journal

The journal is an ordered, append-only collection of feedback blocks. Blank blocks
are dropped; a block identical to one already recorded is dropped too, so
re-requesting the same outline never inflates the tail.

When a later round composes the user message, the order is fixed:

1. the original task;
2. the accumulated journal;
3. the diagnostics of the current phase.

Staging retries and verification healing obtain their prompt through that same
composition, so a retry differs from its earlier attempt only in the trailing
diagnostics. Discovery knowledge paid for once remains available for the whole
turn.

The same dispatcher serves the architect discovery loop during plan refinement and
finding decomposition; that loop enables the already-satisfied explanation and is
described in *Local Plans* and *Structured Code Review*.

### 3.7 Failure feedback and termination

Discovery never fails the turn because a request was wrong. It returns a typed
diagnostic and lets the model correct itself:

- a missing file names the path and points back to the repository outline;
- an invalid regex names the dialect limitation;
- an invalid search path names the traversal or absolute-path rule;
- an empty targeted outline names the requested prefix;
- a breached budget names the three token amounts;
- an uninspectable repository returns a feedback block stating that repository
  facts are unavailable instead of reporting every requested file as missing,
  and the failed listing leaves a warn record;
- a listing over the cap states how many entries were omitted.

The phase terminates when the model stops requesting, a round makes no progress,
or the round budget is exhausted. Even the architect loop, which treats an
already-satisfied round as progress, is bounded by the round budget, so a model
that keeps asking for what it already holds cannot loop forever.

---

## 4. Boundaries & Anti-Patterns ("What it is NOT")

- **It is not an agent loop.** The model asks; the harness validates, executes, and
  decides when the phase ends.
- **It is not a shell.** There is no arbitrary command, no `grep` on disk, and no
  path outside the tracked repository.
- **It is not a context layer.** Discovery findings widen the context through the
  Auto layer; outlines, searches, and documentation live in the journal.
- **It is not persistent history.** The journal is turn-scoped and discarded when
  the turn ends.
- **It is not unbounded.** Rounds, queries, outlines, listings, and tokens all have
  configured ceilings.
- **It is not silent truncation.** Every cap is reported in the feedback block.
- **It is not a replacement for curation.** Loading known files up front removes
  rounds and spends fewer tokens.

---

## 5. Related Chapters

- [The Context Model](context-model.md) — the layers discovery widens.
- [Repository Map and Blinkers](repo-map.md) — the outline that motivates the next
  request.
- [The Autonomous Bounded Turn FSM](../ch01-foundations/bounded-turn-fsm.md) —
  the phase lifecycle and the stable prompt prefix.
- [Configuration and Project Preparation](../ch05-system/configuration.md) —
  discovery budgets and documentation routing.
