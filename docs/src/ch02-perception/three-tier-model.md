# 2. Three-Tier Context Model

The context is the model's working set for one turn: the files it may read, the
files it may modify, and the budgets that keep that set affordable. TAUQE
classifies every file into one of three layers under one shared permission
vocabulary, so the working set stays explicit, auditable, and cheap to widen.

---

## 2.1 Motivation & Invariants ("Why")

### 2.1.1 A working set is not a repository

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

### 2.1.2 Three questions, three layers

A flat list cannot answer why a file is loaded or how long it should stay. TAUQE
separates three questions into three layers:

| Question | Layer |
| --- | --- |
| What does the project declare authoritative? | **Pinned** |
| What did the developer deliberately select? | **User** |
| What did this turn discover? | **Auto** |

Layer membership decides lifetime, who may mutate an entry, and what happens at
the end of the turn.

### 2.1.3 The invariants

1. **A file outside the context is not editable.** The working set is the write
   boundary; a path the model never loaded cannot be patched blindly.
2. **`Editable` wins over `ReadOnly` across all layers.** Effective access is
   merged, never decided by whichever layer mentions the path last.
3. **Pinned entries are configuration-owned.** The interface cannot remove them
   or change their access; only `[context].pinned` decides them.
4. **The Auto layer is turn-scoped; the User layer is deliberate.** Discovery
   accumulates, curation persists.
5. **Budgets are measured, not guessed.** File cost is estimated from size, and
   the aggregate is checked against a configured ceiling before anything is
   added.
6. **Order is deterministic.** Files render in insertion order, never in
   alphabetical order, because the context block is the cached prefix of every
   later round of the same turn.
7. **The prompt prefix, model identity, and provider session travel together.**
   A dialogue keeps one session while the context block stays byte-identical and
   the target model remains unchanged. Session rotation is triggered at two
   deterministic boundaries: when the context block shrinks (removing Auto files
   or clearing context) and when the effective model changes. Altering reasoning
   effort alone or reselecting the same model preserves session affinity.

---

## 2.2 Mental Model & Ontology ("What")

### 2.2.1 Layers

| Layer | Owner | Lifetime | Baseline access | Mutation paths |
| --- | --- | --- | --- | --- |
| **Pinned** | `[context].pinned` | whole session | `read_only` | configuration, hierarchical resolution |
| **User** | developer | across turns | as chosen | add, remove, access change, promote from Auto |
| **Auto** | model discovery | one turn | as requested | context requests, promote, drop, end-of-turn sweep |

### 2.2.2 Access

| Access | Meaning |
| --- | --- |
| `read_only` | knowledge scope: loaded for reference and structural comprehension |
| `editable` | write scope: a declared target for modification |
| absent | unseen: cannot be edited at all until it enters the context |

Effective access merges every layer holding the path: `Editable` if any layer
marks the path editable, otherwise `ReadOnly`, otherwise `None`.

A read-only entry is a knowledge hint, not a wall. When a task genuinely requires
changing such a file, the patch is staged and the file is promoted to `editable`
on apply, which keeps the declared permission honest without wasting a discovery
round. A file absent from every layer is a hard boundary: it must be requested
before it can be touched.

### 2.2.3 Working vocabulary

- **Context item** — one entry: path, access, size in bytes, estimated tokens, and
  owning layer.
- **Effective access** — the merged permission described above.
- **Pinned pattern** — a repository-relative path declared in `[context].pinned`
  and resolved at the repository root and at each ancestor directory of every
  User and Auto file.
- **Pattern batch** — one glob or directory prefix matched against the tracked
  file list, capped by `context.max_pattern_batch`.
- **Revision** — a counter advanced only when layer contents actually change, so a
  no-op mutation never disturbs clients.
- **Estimated tokens** — `size_bytes / 4`, the single estimate every budget uses.
- **Dialogue session** — the provider-level identity an interactive dialogue
  binds its requests to. It routes related requests to the same cache-warm node
  and lives exactly as long as the prompt prefix it warmed.

---

## 2.3 Mechanics & Lifecycle ("How it behaves")

### 2.3.1 Resolution and rendering order

State is emitted in pinned → user → auto order. Each path appears exactly once:
the first owning layer wins, and the merged effective access is applied to that
single row. The aggregate estimated token count is summed over the emitted rows,
and the revision advances only when something actually changed.

The `<context>` block of the prompt is rendered in the same order from readable
files inside the repository. A path that escapes the repository, has been deleted,
or cannot be decoded as UTF-8 is skipped instead of failing the turn.

Insertion order is a contract, not an implementation detail. Alphabetical sorting
would reorder the block between rounds, invalidate the provider's cached prefix,
and make a multi-round turn pay for the whole prefix again.

### 2.3.2 Pinned and hierarchical resolution

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

Pinned entries are configuration-owned. `context/set_access` on a pinned-only path
is refused, `context/remove` never removes a pinned-only path, and a rename or
move through context operations is rejected. A model request for editable access
on a pinned file does not duplicate it: a User-layer override carries `Editable`,
the effective access becomes editable, and the path still renders exactly once.

### 2.3.3 User and Auto lifecycle

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

### 2.3.4 Budgets and pattern intake

Per-file cost is estimated from size, and the assembled context reports an
aggregate `total_estimated_tokens`. `context.files_token_budget` (300,000 by
default) bounds that aggregate. The check runs inside discovery: current tokens
plus the estimated cost of the candidate additions are compared with the budget
before a single file is added. A breach rejects the whole addition set, halts the
round with a diagnostic naming the current, requested, and allowed amounts, and
lets the developer scope the task down instead of watching the turn fail
anonymously.

A whole family of files can be loaded by glob pattern or directory prefix in one
batch. The intake matches the tracked file list, skips paths already curated in
the User layer, sorts matches for determinism, and caps the batch at
`context.max_pattern_batch` (150 by default). When the match set exceeds the cap,
the outcome reports truncation, so the limit is never silent.

Two related budgets belong to other mechanics: `context.max_discovery_rounds`
bounds widening rounds (see *Discovery Engine*), and `context.repomap_token_budget`
bounds the repository outline (see *Repository Map and Blinkers*).

### 2.3.5 Turn merge and reconciliation

A turn reasons over a snapshot of the context. When it completes, the snapshot
merges back into the live state:

- the Auto layer is replaced by the turn's Auto layer, excluding paths already
  owned by the User layer;
- paths that no longer exist on disk are pruned from every layer;
- access upgrades and files created by the turn are mirrored into the live User
  layer;
- hierarchical Pinned entries are recomputed, and file metadata is refreshed.

Context mutations are refused while a turn or batch is running: add (including
pattern intake), remove, access change, and clear answer as an operation in
progress. The merge therefore operates on an untouched base, so the turn's frozen
snapshot can never overwrite or resurrect the developer's live changes.

### 2.3.6 Mutating the working set

The interface exposes a small, typed surface: add one file (optionally into a
specific layer), add a pattern batch, remove one file, change access, clear the
Auto layer, and clear the User layer.

Every mutation follows the same discipline. A no-op does not advance the revision.
Removing a pinned-only path reports no change instead of success. Setting the
access a file already has reports no change. Clearing an empty layer is silent.
This keeps clients honest: a revision bump always means the visible state moved.

---

## 2.4 Boundaries & Anti-Patterns ("What it is NOT")

- **It is not a repository index.** Context lists the files the turn actually
  carries; the repository map and directory listings describe what exists.
- **It is not a security sandbox.** `read_only` is a knowledge hint with a
  deterministic promotion path. The hard boundary is absence from the context,
  not the access attribute.
- **It is not durable memory.** The Auto layer is swept at the end of the turn.
  Turn-local working memory lives in the scratchpad; session memory lives in
  history.
- **It is not one session forever.** The provider session rotates whenever Auto
  files are actually removed or when the effective model changes, because
  neither a shrunken context block nor a different model can reuse the cache its
  warm identifier points at.
- **It does not make the Pinned layer editable.** `[context].pinned` owns those
  entries; the interface cannot add, remove, or re-permission a pinned path.
- **It is not sorted.** Alphabetical order is never imposed, because stable
  insertion order is what keeps the prompt prefix cacheable.
- **It is not unlimited.** A breached budget halts the round with a diagnostic
  instead of silently dropping files.
- **It is not a substitute for curation.** The model can discover files
  autonomously, but loading the task-relevant files up front removes discovery
  rounds and spends fewer tokens.

---

## 2.5 Related Chapters

- [Repository Map and Blinkers](repo-map.md) — the deterministic outline of the
  repository the model reads instead of the whole tree.
- [Discovery Engine](discovery-engine.md) — the bounded rounds that widen the
  context and the journal that records their findings.
- [The Autonomous Bounded Turn FSM](../ch01-foundations/bounded-turn-fsm.md) —
  where the context block is rendered and why the prefix stays stable.
- [Configuration and Project Preparation](../ch05-system/configuration.md) —
  `[context]` budgets, pinned patterns, and the on-demand outline switch.
