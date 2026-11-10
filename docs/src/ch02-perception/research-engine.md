# Research Engine

Research is the phase that converts "the task depends on a contract I have not
read" into verified knowledge. It widens the working set through typed requests,
runs each request deterministically over tracked files or over the external
documentation the project declares, and keeps every finding available for the rest
of the turn. It is bounded by rounds, budgets, and request caps, so exploration
always terminates.

---

## 1. Motivation & Invariants ("Why")

### 1.1 Unknown unknowns

A task arrives with an incomplete map. The model can see contours, but it cannot
know which module defines a trait, which implementation is relevant, or which
chapter states an invariant. Guessing produces plausible patches against invented
contracts. Research replaces guesses with requests: open the file, search the
tracked tree, outline the subsystem, list the directory, load the documentation
chapter, mount a declared external document.

### 1.2 Why the developer does not curate by hand

Manual context logistics is a failure mode, not a workflow. The developer states
the task; the model asks for what it lacks; the harness decides whether the request
names tracked material or a declared external source, whether it fits the budget,
and whether the round cap still allows it.

### 1.3 The invariants

1. **Declared material only.** Research reads tracked files, the harness-owned
   sandboxes, and the external documents the project declares in `[[outer_docs]]`.
   An unknown path becomes explicit "not found" feedback instead of a guess.
2. **Every round is bounded.** Rounds, searches, outlines, listings, and the single
   context token budget all have configured ceilings.
3. **Deterministic execution.** Searches run in Rust over tracked files, outlines
   come from Tree-sitter, documentation comes from embedded chapters, and a
   declared document is materialized from its declared source exactly once.
4. **Nothing learned is lost.** Findings that are not context files accumulate in a
   turn-scoped journal that survives every later phase.
5. **The cached prefix does not move.** Round findings extend the user message;
   they never rewrite the stable block of the system prompt.
6. **A breach halts with a diagnostic.** No budget silently drops a requested file.
7. **The phase is deterministic; the content is generative.** The model chooses
   what to ask for, never which stage runs next.
8. **A refusal keeps its audience.** A trust decision of the developer's
   declaration and a failed transfer are never merged into one diagnostic: the
   first is answered by the developer, the second by the model.
9. **Public material needs no entry.** A declared address that resolves to a public
   Internet address is materialized without a workspace entry, because published
   material is public. The developer's declaration guards the host instead: local
   paths outside the repository, and non-public networks.
10. **A transfer is guarded before the body.** A declared `Content-Length` above the
    snapshot ceiling and a non-textual `Content-Type` refuse the response on its
    headers, a body of unknown length is bounded while it is read, and only valid
    UTF-8 text becomes a cache file.

---

## 2. Mental Model & Ontology ("What")

### 2.1 A round

A research round is one model interaction that may carry any number of typed
research commands. The round first materializes the declared external documents it
addresses, then the dispatcher resolves repository facts once, hands them to every
handler that owns a command, and folds the results into one outcome.
A round carries research commands only. Discovery is its own operational branch
of the capability surface, so a round that opens files or searches the tree
proposes no change set, and a round that patches the tree asks for nothing. A
response that mixes the two is a protocol defect: the turn keeps its change set
and reports the discovery request as the defect to correct in a later round (*The
Autonomous Bounded Turn FSM* §3.3.2), so exploration never silently costs the
model the edits it already wrote.

### 2.2 Commands and handlers

| Command | Handler | What it returns |
| --- | --- | --- |
| `<context_request path access>` | context request | files added to the Auto layer, or "not found" |
| `<repo_search path regex>` | repository search | matching tracked files with hit counts |
| `<repo_dirs />` | directory tree | the compact folder outline |
| `<repo_map path>` | targeted outline | the symbol map of one subtree |
| `<dir_list path recursive>` | directory listing | tracked files of one directory |
| `<tauqe_doc_request topic>` | documentation | the embedded chapter of one topic |
| `<outer_doc_request id>` | outer documentation | the declared document the identifier names, or the relative document below its root |

Each handler implements `ResearchCapabilityHandler` and owns its validation, its
disk work, and its feedback text. The turn loop dispatches polymorphically instead
of branching on individual command types, so a capability cannot drift from its
validation or its explanation.

### 2.3 Feedback blocks

Findings return as delimited blocks:

- `<context_feedback>` — missing files, an explanation when requested files are
  already loaded, and a notice when the repository could not be inspected.
- `<search_feedback path="..." regex="...">` — per-file hit counts, an omission
  count, or a syntax diagnostic.
- `<repo_dirs_feedback>` — the compact folder outline.
- `<repomap_feedback path="...">` — a targeted outline.
- `<dir_list_feedback path="..." recursive="...">` — a directory listing.
- `<system_documentation>` — authoritative chapters of the harness itself.
- `<outer_docs_feedback>` — a declared external document that could not be
  materialized, worded for the reader who can act on the failure (3.7).

### 2.4 The journal

Non-file findings cannot enter the context block: that block is the authoritative
set of context files, and mixing outlines or documentation into it would pollute
patching. They accumulate instead in the turn-scoped `TurnResearchJournal`, an
append-only, de-duplicated tail of the user message.

### 2.5 Budgets

The phase obeys these ceilings:

| Bound | Setting or constant | Default |
| --- | --- | --- |
| Research rounds per turn | `context.max_research_rounds` | 5 |
| Aggregate context tokens | `context.token_budget` | 300,000 |
| Outline tokens | `context.repomap_token_budget` | 4,000 |
| Searches per round | `context.max_search_requests_per_round` | 5 |
| Matching files reported | `context.max_matched_files_per_search` | 50 |
| Searchable file size | `MAX_SEARCHABLE_FILE_SIZE` | 2 MiB |
| Searchable text per round | `MAX_SEARCHABLE_CORPUS_BYTES` | 64 MiB |
| Targeted outlines per round | `context.max_repo_map_requests_per_round` | 3 |
| Directory listing entries | `context.max_dir_list_entries` | 100 |
| Project command output | `context.max_feedback_chars` | 4,000 |
| Turn scratchpad block | `context.max_scratchpad_chars` | 4,000 |
| One materialized snapshot | `MAX_DOCUMENT_BYTES` | 4 MiB |

A project raises a ceiling in `tauqe.toml` or in a `tauqe.d` module instead of
recompiling the engine.

---

## 3. Mechanics & Lifecycle ("How it behaves")

### 3.1 The round

1. The model emits research commands, with or without surrounding text.
2. The round materializes the declared external documents it addresses before the
   dispatcher looks for their files (3.5).
3. The dispatcher resolves the tracked file list once, off the async executor
   thread, and only when a command needs it. A failed resolution ends the round
   with an inspection-failure feedback block.
4. Every registered handler that owns at least one command executes in dispatch
   order and folds its findings into one outcome.
5. A token breach stops the dispatcher immediately: later handlers are not run
   against a context that was never widened.
6. The outcome reports whether the round made progress, and the turn either asks
   again or proceeds to proposal while the round budget lasts.

### 3.2 Opening files into context

For each requested path the dispatcher normalizes it and requires it to exist among
tracked files, among the harness-owned sandboxes, or already in context. A path
that fails becomes a missing-file diagnostic. A path whose current access already
satisfies the request becomes an "already satisfied" note instead of a duplicate
addition.

Candidate additions are priced as their estimated content plus the wrapper each
`<file>` element renders, and compared with the remaining context token budget; the
number of files a round admits has no separate ceiling. A breach rejects the whole
addition set and reports the current, requested, and allowed amounts. Additions
land in the Auto layer, upgrade access when needed, and never duplicate a pinned
file.

The already-satisfied note is deliberately aggregate-aware: it is emitted only when
the round added no files, ran no searches, and the caller allows the explanation. A
plain developer turn does not need to be told about files it already asked for; the
architect research loop does, because a re-request is a legitimate signal that the
Architect is still reasoning over what it holds. Even there the note is granted a
single round, so a repeated request cannot be traded for an unbounded dialogue.

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
- A round retains at most `MAX_SEARCHABLE_CORPUS_BYTES` (64 MiB) of searchable
  text. Files enter the shared corpus while they fit, and the files left over are
  reported in the block as unsearched, so a bounded round never reads as a
  complete absence of matches.

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

### 3.5 Declared external documents

A project mounts material it does not own through `[[outer_docs]]`, declared in
`tauqe.toml` or in a `tauqe.d` module. Each entry names a mount point (`id`), a label, and
exactly one source through the single `source` key: an `http(s)` address is fetched over the
network, and any other value names a local file. The registry never touches a declared
source until a round request addresses it.

Resolution is a lazy pre-step. Only an outer-document request or a context request naming a
path under `.tauqe/cache/outer_docs/` consults the registry, so an ordinary round that opens
repository files never reads a declaration. An outer-document request names the root
document as `{id}` and a document below it as `{id}/{relative}`. A context request naming a
cache path is reduced to the declaration it addresses. An already materialized snapshot is
answered as is, without a second transfer and without a copy, and before the trust rule is
consulted: the snapshot is already local material, so no later decision has anything to
authorize.

The root document is answered as its original text. The harness parses no index of it, so
the model follows the relative Markdown links the text carries and requests the documents
they name, one per round. A relative document resolves below the directory that holds the
root, and the jail refuses any request that climbs above it.

Materialization has a fixed sequence:

- an existing snapshot is served immediately, so the material the model reasoned
  about stays stable for the whole task;
- a local source inside the repository is the project's own material and needs no
  entry, while a source outside it is read only from a directory the developer's
  declaration allows;
- a remote source is resolved by the harness itself, and a public address is
  fetched without an entry: `refs-trust.toml` guards the host, not the public
  Internet. Every address it reaches must be public unless the declaration allows a
  non-public network;
- the response is guarded on its headers before the body is read: a declared
  `Content-Length` above `MAX_DOCUMENT_BYTES` abandons the transfer, and a media
  type outside the textual family is refused as binary material;
- the transfer races the turn's cancellation signal and is bounded by the
  per-request wall-clock budget, by `MAX_DOCUMENT_BYTES`, and by the bytes
  accumulated from a body that declares no length;
- the bytes are validated as UTF-8 and land through the atomic replacement, so a
  reader observes either the previous complete snapshot or the new one.

A document a round requested enters the context read-only, priced exactly like a
repository file, and remains resident for every later round of the same task. It is released
together with the files a round discovered once the turn's change set is committed or the
isolated step ends. A plan step declares the same kind of document through the `outer_docs`
field of its leaf, and the harness mounts it before the step turn starts (*Local Plans*).

### 3.6 Requesting harness documentation

`<tauqe_doc_request topic="..." />` returns embedded chapters for `overview`, `context`,
`workflow`, `git`, `interface`, `review`, `plan`, `config`, or `all`. The model asks
for the authoritative chapter instead of reconstructing behavior from partial source
memory.

The capability belongs to the interactive dialogue, where the developer prepares a
project and chooses a workflow. An isolated step execution carries no documentation
request at all: its turn implements one approved architectural decision, so the
Architect already settled *what* must be built and the step needs the repository
rather than the reference manual. The isolation follows from the capability surface
itself, so a turn without the request renders no documentation rule, declares no
documentation field in its schema, and decodes no documentation tag; the prompt
never advertises a channel the decoder would reject. A declared external document
stays reachable in such a turn, because it enters the context through a context
request rather than through the harness documentation channel.

### 3.7 Refusals, deliveries, and their audiences

The most important distinction of the phase is who can act on a failure.

A **trust refusal** is a decision of the developer's declaration: the workspace has
no entry, a local source lies outside every declared root, a remote target is not a
public address, or a declared address carries credentials. A public address is not
gated at all, so a public source never produces one. No round can satisfy a refusal,
so the outcome keeps it as a typed entry beside the exact cause and the
ready-to-paste `[[workspace]]` block that would authorize the source, and the
model's feedback forbids routing around the prohibition:

```text
<outer_docs_feedback>
The external document `<path>` was refused by the developer's trust declaration:
<reason>
This decision belongs to the developer and cannot be satisfied from inside the
turn. Report the missing document and the declaration it needs instead of retrying
the request: do not request the same path again, and do not look for another route
to the same material.
</outer_docs_feedback>
```

A **delivery failure** — an HTTP status, a timeout, a cancelled turn, an oversized
body, a non-textual media type, a body that is not UTF-8, an unreadable path —
addresses the model, which works with the material it already holds and may attempt
the same request again in a later round. It travels beside the trust refusals but
never as one, so a reachability problem is never reported as a prohibition.

### 3.8 The turn journal

The journal is an ordered, append-only collection of feedback blocks. Blank blocks
are dropped; a block identical to one already recorded is dropped too, so
re-requesting the same outline never inflates the tail.

When a later round composes the user message, the order is fixed:

1. the original task;
2. the accumulated journal;
3. the diagnostics of the current phase.

Staging retries and verification healing obtain their prompt through that same
composition, so a retry differs from its earlier attempt only in the trailing
diagnostics. Research knowledge paid for once remains available for the whole turn.

The same dispatcher serves the structured Architect synthesis contour in
`crates/core`, which projects plan horizons and decomposes review groups. That
contour is a policy layer on the shared turn kernel, so its rounds are the kernel's
rounds and its findings land in the same journal; the already-satisfied explanation
is enabled there, because a re-request is a legitimate signal that the Architect is
still reasoning over what it holds. The server no longer runs a research loop of its
own: it resolves the provider, relays the contour's events, and records the outcome.

### 3.9 Failure feedback and termination

Research never fails the turn because a request was wrong. It returns a typed
diagnostic and lets the model correct itself:

- a missing file names the path and points back to the repository outline;
- an invalid regex names the dialect limitation;
- an invalid search path names the traversal or absolute-path rule;
- an empty targeted outline names the requested prefix;
- a breached budget names the three token amounts;
- an uninspectable repository returns a feedback block stating that repository
  facts are unavailable instead of reporting every requested file as missing, and
  the failed listing leaves a warn record;
- a listing over the cap states how many entries were omitted;
- a refused declared document names the developer's decision and the declaration
  that would authorize the source, and forbids a retry inside the turn;
- a failed delivery states what the harness could not obtain and still allows the
  same request in a later round;
- a response refused for its declared length, its media type, or its text encoding
  names the guard it violated and the ceiling it crossed, so the next request aims
  at material the harness can hold;
- a request that climbs above the declared root is refused, naming the root and the rule
  it enforces;
- an undeclared mount point names the declaration that would have to add it.

The phase terminates when the model stops requesting, a round makes no progress, or
the round budget is exhausted. Even the architect loop, which treats an
already-satisfied round as progress, is bounded by the round budget, so a model that
keeps asking for what it already holds cannot loop forever.

---

## 4. Boundaries & Anti-Patterns ("What it is NOT")

- **It is not an agent loop.** The model asks; the harness validates, executes, and
  decides when the phase ends.
- **It is not a shell.** There is no arbitrary command, no `grep` on disk, and no
  path outside the tracked repository or a declared external source.
- **It is not a context layer.** Research findings widen the context through the
  Auto layer; outlines, searches, and documentation live in the journal.
- **It is not persistent history.** The journal is turn-scoped and discarded when
  the turn ends.
- **It is not unbounded.** Rounds, queries, outlines, listings, and tokens all have
  configured ceilings.
- **It is not a branch of the proposal.** A research round carries discovery
  commands only; a round that proposes a change set carries none, and a mixed
  round is a protocol defect the turn resolves in favor of its change set.
- **It is not silent truncation.** Every cap is reported in the feedback block.
- **It is not a replacement for curation.** Loading known files up front removes
  rounds and spends fewer tokens.
- **It is not a network client on demand.** Reading a declaration fetches nothing;
  only a request from a round materializes a declared document.
- **It is not an authority over the host.** A repository declares what it wants;
  the developer's trust declaration decides which host material the harness may
  read, and no round can widen it. The public Internet is not host material, so a
  public address needs no entry.
- **It is not an unguarded fetch.** A public address is still refused when it
  declares too large a length, answers with a binary media type, or delivers bytes
  that are not UTF-8.

---

## 5. Related Chapters

- [The Context Model](context-model.md) — the layers research widens, and the
  read-only layer a declared document enters.
- [Repository Map and Blinkers](repo-map.md) — the outline that motivates the next
  request.
- [The Autonomous Bounded Turn FSM](../ch01-foundations/bounded-turn-fsm.md) — the
  phase lifecycle and the stable prompt prefix.
- [Configuration and Project Preparation](../ch05-system/configuration.md) — the
  modular layers, the `[[outer_docs]]` declarations, and the discovery budgets.
