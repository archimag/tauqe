# The Context Model

The context is the model's working set for one turn: the files it may read, the
files it may modify, and the budgets that keep that set affordable. TAUQE
classifies every file into one of four layers under one shared permission
vocabulary, so the working set stays explicit, auditable, and cheap to widen.

---

## 1. Motivation & Invariants ("Why")

### 1.1 A working set is not a repository

Sending the whole repository is neither possible nor useful. A large tree exceeds
any context window, and even a small one degrades attention: unrelated files
dilute the task, raise latency and cost, and invite patches that touch code the
request never mentioned. The opposite extreme fails just as reliably. When the
developer must add every file by hand, mark it read-only, and guess which types
the model will need, the tool stops reducing cognitive load and turns the engineer
into a context logistician.

The working set has to be *minimal and sufficient*: every file the task needs,
nothing it does not, and a deterministic way to widen the set when the model
discovers a missing contract.

### 1.2 Four questions, four layers

A flat list cannot answer why a file is loaded or how long it should stay. TAUQE
separates four questions into four layers:

| Question | Layer |
| --- | --- |
| What does the project declare authoritative? | **Pinned** |
| What does the task quote from outside the repository? | **Refs** |
| What did the developer deliberately select? | **User** |
| What did this turn discover? | **Auto** |

Layer membership decides lifetime, who may mutate an entry, and what happens at
the end of the turn.

### 1.3 The invariants

1. **A file outside the context is not editable.** The working set is the write
   boundary; a path the model never loaded cannot be patched blindly.
2. **`Editable` wins over `ReadOnly` across all layers.** Effective access is
   merged, never decided by whichever layer mentions the path last. The Refs
   layer never contributes `Editable`: a reference snapshot is read-only by
   construction.
3. **Pinned entries are configuration-owned.** The interface cannot remove them
   or change their access; only `[context].pinned` decides them.
4. **The Auto layer is turn-scoped; the User layer is deliberate.** Discovery
   accumulates, curation persists.
5. **The Refs layer is registry-managed and task-scoped.** A document enters it
   only through the materialization a declared external reference produced, it
   stays read-only for the whole task, and nothing but the layer's own token
   budget evicts it automatically.
6. **Budgets are measured, not guessed.** File cost is its estimated content plus
   the estimated wrapper its `<file>` element renders, and the aggregate is checked
   against a configured ceiling before anything is added.
7. **Order is deterministic and belongs to the layout policy.** Every entry
   carries a monotonic insertion watermark, so a file's position derives from
   when it arrived, never from its name or from hash iteration order. The
   semantic policy renders layer order with insertion order inside a layer; the
   chronological policy renders the global insertion order. Neither of them ever
   sorts alphabetically, because a re-sorted prompt invalidates the cached
   prefix between rounds.
8. **The prompt prefix, model identity, and provider session travel together.**
   A dialogue keeps one session while the context block stays byte-identical and
   the target model remains unchanged. Session rotation is triggered at two
   deterministic boundaries: when the context block shrinks — clearing the Auto
   layer, dropping a reference document, or resetting the context — and when the
   effective model changes. Altering reasoning effort alone or reselecting the
   same model preserves session affinity.

---

## 2. Mental Model & Ontology ("What")

### 2.1 Layers

| Layer | Owner | Lifetime | Baseline access | Mutation paths |
| --- | --- | --- | --- | --- |
| **Pinned** | `[context].pinned` | whole session | `read_only` | configuration, hierarchical resolution |
| **Refs** | external reference registry | whole task | `read_only` | materialization, LRU eviction, explicit removal |
| **User** | developer | across turns | as chosen | add, remove, access change, promote from Auto |
| **Auto** | model discovery | one turn | as requested | context requests, promote, drop, end-of-turn sweep |

### 2.2 Access

| Access | Meaning |
| --- | --- |
| `read_only` | knowledge scope: loaded for reference and structural comprehension |
| `editable` | write scope: a declared target for modification |
| absent | unseen: cannot be edited at all until it enters the context |

Effective access merges every layer holding the path: `Editable` if any layer
marks the path editable, otherwise `ReadOnly`, otherwise `None`. A reference
document is read-only by construction, so it contributes `ReadOnly` like any
other read-only entry and is never promoted to `editable`.

A read-only entry is a knowledge hint, not a wall. When a task genuinely requires
changing such a file, the patch is staged and the file is promoted to `editable`
on apply, which keeps the declared permission honest without wasting a discovery
round. A file absent from every layer is a hard boundary: it must be requested
before it can be touched.

### 2.3 Working vocabulary

- **Context item** — one entry: path, access, size in bytes, estimated tokens, and
  owning layer.
- **Effective access** — the merged permission described above.
- **Pinned pattern** — a repository-relative path declared in `[context].pinned`
  and resolved at the repository root and at each ancestor directory of every
  User and Auto file.
- **Pattern batch** — one glob or directory prefix matched against the tracked
  file list; the file token budget is the only measure of how much one batch may
  admit.
- **Reference document** — a snapshot the harness materialized from a declared
  external source and admitted read-only into the Refs layer.
- **LRU watermark** — the monotonic counter value recorded on the most recent
  request for a reference document; eviction compares those watermarks rather
  than arrival order, so a document requested again stays resident.
- **Revision** — a counter advanced only when layer contents actually change, so a
  no-op mutation never disturbs clients.
- **Estimated tokens** — `size_bytes / 4`, the single estimate every budget uses.
- **Dialogue session** — the provider-level identity an interactive dialogue
  binds its requests to. It routes related requests to the same cache-warm node
  and lives exactly as long as the prompt prefix it warmed.

---

## 3. Mechanics & Lifecycle ("How it behaves")

### 3.1 Resolution and rendering order

State is emitted in pinned → refs → user → auto order. Each path appears exactly
once: the first owning layer wins, and the merged effective access is applied to
that single row. The aggregate estimated token count is summed over the emitted
rows, and the revision advances only when something actually changed.

The context files of a turn are rendered from readable files inside the repository,
and a path that escapes the repository, has been deleted, or cannot be decoded as
UTF-8 is skipped instead of failing the turn. The order they travel in is the
business of the active layout policy:

- the **semantic** policy renders one `<context>` block in layer order, with
  insertion order inside each layer;
- the **chronological** policy renders transcript order, which is the order of the
  insertion watermark every entry carries.

An **insertion watermark** is the monotonic counter value a path receives when it
first enters the context and keeps for as long as it stays there, so a re-add
upgrades its access in place instead of moving it to the tail of the timeline. The
watermark is what lets the chronological policy place a file where it actually
arrived rather than where its layer happens to sort it.

Both orders are deterministic and both grow append-only: a file that enters the
context mid-turn extends the sequence at its tail. Alphabetical sorting would
reorder the prompt between rounds, invalidate the provider's cached prefix, and make
a multi-round turn pay for the whole prefix again.

### 3.2 Pinned and hierarchical resolution

The Pinned layer resolves in two passes:

1. Every pattern is resolved against the repository root, which keeps project-wide
   guidelines present in every prompt.
2. For every file in the User or Auto layer, each parent directory up to the
   repository root resolves every pattern as a relative path. Existing matches
   join the Pinned layer, deduplicated, read-only, and verified never to escape
   the repository root.

The consequence is practical. Loading `crates/core/src/workflow/fsm.rs` brings
`crates/core/Conventions.md` into the prompt automatically, because the subsystem
conventions live at an ancestor level. Subsystem rules arrive exactly when the
subsystem code does.

The second pass covers the User and Auto layers only. A reference document lives
in the harness-owned cache directory rather than in a project subsystem, so
requesting one resolves no hierarchical pattern.

Pinned entries are configuration-owned. `context/set_access` on a pinned-only path
is refused, `context/remove` never removes a pinned-only path, and a rename or
move through context operations is rejected. A model request for editable access
on a pinned file does not duplicate it: a User-layer override carries `Editable`,
the effective access becomes editable, and the path still renders exactly once.

### 3.3 User and Auto lifecycle

Adding a file to the User layer evicts the same path from the Auto layer, so a
curated path is never listed twice and never disappears with the end-of-turn
sweep. A file the model creates on disk during a turn joins the User layer for the
same reason: new work must survive the sweep and stay editable in later turns.

The Auto layer accumulates monotonically during a turn. Discovery never removes a
previously discovered file, because later rounds may still depend on it. The
developer can promote an entry to the User layer, drop a single entry, or clear the
whole layer. When the turn ends, the transient layer is swept and its surviving
contents merge into the live context.

Access only moves upward in the Auto layer: a discovery request can upgrade a
read-only entry to editable, never the reverse. A read-only request against an
already editable file is a no-op.

The layer is also the part of the prompt an interactive dialogue reuses across
rounds, so its lifecycle is tied to the provider's session affinity. A dialogue
keeps one session identifier while the context block stays byte-identical and
the effective model remains unchanged, which is what lets a series of answers
reuse the prefix cache warmed by its predecessor. Rotation occurs on two
deterministic events: when clearing the Auto layer shrinks the context block,
and when switching to a different effective model. Clearing the layer — from the
Context view, through `context/clear`, or at the end of a turn that changed
files — shrinks that block, so the same decision rotates the identifier and the
next request opens a fresh session. The rotation follows the boolean the clear
reports, so a repeated clear on an already empty layer leaves both the block and
the session untouched. Switching the effective model likewise rotates the
identifier (`maybe_rotate_session_on_model_change`), because physical KV caches
are model-specific; modifying only reasoning effort or reselecting the identical
model keeps the session intact.

### 3.4 The Refs layer lifecycle

The Refs layer holds material the project does not own: a document an external
reference declared and the harness materialized into the isolated cache. Its
manifest, its cache, and its navigation rules belong to *The External Reference
Registry*; here it is the fourth layer of the working set and nothing more.

**Admission.** A document enters through the registry and only through it. A
round that requests a path under the tracked registry directory is resolved to a
declared entry, the entry is materialized, the request is rewritten to its cache
path, and the resulting file joins the layer read-only whatever access the request
named. A manual addition is refused, because the layer is registry-managed.

**Cost and eviction.** A document is estimated from its size, like every other
entry. One document that alone exceeds `context.refs_token_budget` is refused with
a diagnostic naming the budget, instead of evicting every other document and
failing anyway. A new admission keeps the layer inside that budget by evicting the
least recently requested document until it fits. A repeated request refreshes only
the LRU watermark of that document: the snapshot is immutable for the whole task,
so no rendered byte changes and the revision stays put.

**Persistence.** The layer is context rather than transcript, and that single fact
explains its whole lifecycle:

- history compaction rewrites history entries and never touches the layer, so a
  snapshot cannot be summarized away;
- sweeping the Auto layer at the end of a turn leaves the layer untouched, so a
  document outlives the round that requested it;
- a turn merge folds the documents the turn materialized into the live layer, and
  a path held on both sides keeps the more recent LRU watermark;
- pruning drops a document whose cached file disappeared;
- clearing the whole context drops every document with it.

**What is refused.** Access cannot be changed, a document cannot be renamed or
moved, and no operation promotes a snapshot to editable. An explicit removal is
the one mutation the developer owns, because dropping one document is a deliberate
act rather than a curation of the project.

### 3.5 Budgets and pattern intake

Per-file cost is the estimated content plus the estimated wrapper its own `<file>`
element renders — the opening header carrying its path and access attribute and the
closing footer — priced by the same builder that renders the `<context>` block, so the
estimate and the rendered element cannot disagree. The assembled context reports an
aggregate `total_estimated_tokens`, and `context.files_token_budget` (300,000 by
default) bounds that aggregate. The check runs inside discovery: current tokens plus
the estimated cost of the candidate additions are compared with the budget
before a single file is added. A breach rejects the whole addition set, halts the
round with a diagnostic naming the current, requested, and allowed amounts, and
lets the developer scope the task down instead of watching the turn fail
anonymously.

A whole family of files can be loaded by glob pattern or directory prefix in one
batch. The intake matches the tracked file list, skips paths already curated in
the User layer, sorts matches for determinism, and admits them until
`context.files_token_budget` is exhausted. The number of files is never a limit of
its own: a thousand small headers can cost less than one generated bundle, so only
the priced total decides. When the budget drops a match, the outcome reports
truncation, so the omission is never silent.

Three further budgets belong to neighbouring mechanics:
`context.max_discovery_rounds` bounds widening rounds (see *Discovery Engine*),
`context.repomap_token_budget` bounds the repository outline (see *Repository Map
and Blinkers*), and `context.refs_token_budget` (50,000 by default) bounds the
aggregate cost of the Refs layer (see *The External Reference Registry*).

### 3.6 Turn merge and reconciliation

A turn reasons over a snapshot of the context. When it completes, the snapshot
merges back into the live state:

- the Auto layer is replaced by the turn's Auto layer, excluding paths already
  owned by the User layer;
- the reference documents the turn materialized are folded into the Refs layer,
  and a path present on both sides keeps the more recent LRU watermark;
- paths that no longer exist on disk are pruned from every layer;
- access upgrades and files created by the turn are mirrored into the live User
  layer;
- hierarchical Pinned entries are recomputed, and file metadata is refreshed.

Context mutations are refused while a turn or batch is running: add (including
pattern intake), remove, access change, and clear answer as an operation in
progress. The merge therefore operates on an untouched base, so the turn's frozen
snapshot can never overwrite or resurrect the developer's live changes.

### 3.7 Mutating the working set

The interface exposes a small, typed surface: add one file (optionally into a
specific layer), add a pattern batch, remove one file, change access, clear the
Auto layer, clear the Refs layer, clear the User layer, and clear everything.

Every mutation follows the same discipline. A no-op does not advance the revision.
Removing a pinned-only path reports no change instead of success. Setting the
access a file already has reports no change. Clearing an empty layer is silent.
This keeps clients honest: a revision bump always means the visible state moved.

---

## 4. Boundaries & Anti-Patterns ("What it is NOT")

- **It is not a repository index.** Context lists the files the turn actually
  carries; the repository map and directory listings describe what exists.
- **It is not a security sandbox.** `read_only` is a knowledge hint with a
  deterministic promotion path. The hard boundary is absence from the context,
  not the access attribute.
- **It is not durable memory.** The Auto layer is swept at the end of the turn and
  the Refs layer belongs to the task. Turn-local working memory lives in the
  scratchpad; session memory lives in history.
- **It is not a place for a turn to choose its own sources.** The Refs layer holds
  only what the manifest declares; widening it is a declaration, never a context
  operation.
- **It is not one session forever.** The provider session rotates whenever the
  context block shrinks or the effective model changes, because neither a
  shrunken context block nor a different model can reuse the cache its warm
  identifier points at.
- **It does not make the Pinned layer editable.** `[context].pinned` owns those
  entries; the interface cannot add, remove, or re-permission a pinned path.
- **It is not sorted.** Alphabetical order is never imposed, because stable
  insertion order is what keeps the prompt prefix cacheable.
- **It is not unlimited.** A breached budget halts the round with a diagnostic
  instead of silently dropping files.
- **It is not bounded by a file count.** The file token budget is the only measure
  of volume; the number of files one request may admit has no separate ceiling.
- **It is not a substitute for curation.** The model can discover files
  autonomously, but loading the task-relevant files up front removes discovery
  rounds and spends fewer tokens.

---

## 5. Related Chapters

- [The External Reference Registry](reference-registry.md) — the manifest, the
  cache, and the layer whose place in the ontology this chapter states.
- [Repository Map and Blinkers](repo-map.md) — the deterministic outline of the
  repository the model reads instead of the whole tree.
- [Discovery Engine](discovery-engine.md) — the bounded rounds that widen the
  context and the journal that records their findings.
- [The Autonomous Bounded Turn FSM](../ch01-foundations/bounded-turn-fsm.md) —
  where the context block is rendered and why the prefix stays stable.
- [Configuration and Project Preparation](../ch05-system/configuration.md) —
  `[context]` budgets, pinned patterns, and the on-demand outline switch.
