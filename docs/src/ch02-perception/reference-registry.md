# The External Reference Registry

Sovereign concept chapter: the declarative manifest `.tauqe/refs/refs.md`, the immutable
snapshot cache under `.tauqe/cache/refs/`, and the persistent read-only Refs context layer
through which a turn reads material the project does not own.

A project routinely reasons against documents it did not write: an upstream API note, a
protocol summary, a vendored `llm.txt` transcript. None of them belongs in a commit, and none
of them can be handed to the model as a URL. The registry declares that material, the cache
holds the copy the model actually reads, and the Refs layer keeps it for the rest of the
task.

---

## 1. Motivation & Invariants ("Why")

### 1.1 External material has no address in a repository

A quoted document has two possible addresses, and neither is a repository path: an
`http://`/`https://` URL, or an absolute path on one developer's machine. Committing its
bytes would put a file nobody maintains under review forever. Pasting the whole corpus into
the prompt would spend the context budget on material the task may never read.

The remaining shortcut is worse. Letting the model fetch a URL is arbitrary network access,
refused for the same reason the harness refuses an arbitrary shell: an untyped channel to
the outside world is not a protocol operation.

The registry replaces every shortcut with a declaration. The developer writes down where the
document comes from, the harness materializes a copy at a deterministic path inside the
repository, and the model requests that path exactly like any other context file.

### 1.2 A snapshot is a promise of stability

A bounded turn reasons across several rounds: discovery, the proposal, staging retries, and
verification healing. If the document under its reasoning could change between two rounds,
the later round would argue about bytes the earlier round never saw, and neither round would
be reproducible afterwards.

The registry therefore materializes each entry once per task and treats the result as
immutable. A repeated request is answered from the cache without a second download, and the
harness never refreshes a snapshot on its own initiative.

### 1.3 Relative navigation must stay outside the working tree

A referenced document legitimately links its siblings: `./protocol.md`, `../vendor/api.md`.
Resolving such a link against the working tree would turn quoted external material into a way
of naming any repository path.

The cache answers that. A snapshot is stored under its registry name inside
`.tauqe/cache/refs/`, so a relative link inside it resolves inside the same isolated tree,
which only ever holds snapshots. The model navigates the corpus by requesting those cache
paths, and discovery admits them by existence because Git never lists them.

### 1.4 The invariants

1. **One declaration.** `.tauqe/refs/refs.md` is the only source of an external reference.
   Nothing else names one, and reading it fetches nothing.
2. **The manifest declares; it never stores.** It carries titles and addresses, never
   content.
3. **A registry name is the identity of an entry.** It is a clean repository-relative path,
   it names the snapshot inside the cache, and a repeated name keeps its first declaration.
4. **One snapshot per task.** A cache file that exists is answered byte for byte; the harness
   never refreshes it, downloads it again, or rewrites it.
5. **Snapshot material is read-only.** It cannot be promoted to editable, renamed, moved, or
   re-permissioned.
6. **The cache never joins the change set.** It lives under `.tauqe/`, which every
   transaction excludes, so no snapshot reaches a commit or a verification run.
7. **Fetching happens inside the bounded turn.** It races the turn's cancellation signal and
   reports a compact reason instead of panicking.
8. **The Refs layer is context, not transcript.** History compaction never touches it, and
   only `refs_token_budget` can evict a document from it.
9. **Eviction follows the request, not the arrival.** The least recently requested document
   leaves first, so a document the dialogue keeps asking for stays resident.
10. **A refused declaration costs one line.** An unusable name, an unusable source, and a
    duplicate are each dropped and reported, while the rest of the registry stays usable.
11. **The catalogue is paid for, not the corpus.** The manifest is pinned automatically, so
    the model always reads what is declared while only the documents it requests cost
    tokens.
12. **A document larger than the whole budget is refused.** Admitting it would evict every
    other document and still leave the layer over budget.

---

## 2. Mental Model & Ontology ("What")

### 2.1 Terms

- **Registry manifest** — the tracked file `.tauqe/refs/refs.md`: the declaration of every
  external reference the project recognizes.
- **Entry** — one declared document: a title, a registry name, and a source.
- **Registry name** — the clean repository-relative path that identifies an entry and names
  its snapshot inside the cache.
- **Source** — where the content comes from: an `http://`/`https://` URL or an absolute local
  path.
- **Snapshot** — the bytes the harness materialized from a source, stored at
  `.tauqe/cache/refs/<name>`.
- **Cache path** — the repository-relative path of a snapshot: what a request is rewritten
  to, and what the model reads.
- **Refs layer** — the context layer holding snapshots read-only for the whole task.
- **Showcase** — the automatically pinned manifest entry that renders the catalogue into the
  prompt.
- **LRU watermark** — the monotonic counter value recorded on the most recent request for a
  document; the eviction order compares it.

### 2.2 The manifest grammar

The manifest is a declared, line-oriented data format, not prose that a reader interprets.
Exactly one entry occupies one line:

```text
- [title](name) = source
```

- `title` is the human-readable label rendered in the showcase. It carries no meaning to the
  harness.
- `name` is a clean repository-relative path written with `/`: non-empty, never absolute,
  carrying neither a `.` nor a `..` segment and no backslash. It identifies the entry, so the
  snapshot is stored under exactly this path.
- `source` is either an `http://`/`https://` URL with a host part, or an absolute local path.
  Anything else — a repository-relative path, a malformed address, an empty value — is
  rejected instead of guessed at.

Whitespace around the delimiters is insignificant. Every line that does not carry this shape
— a heading, a paragraph, a markdown link without a source — is prose: it has no registry
meaning and is skipped silently, so a manifest can document itself around its entries.

### 2.3 What a rejected line looks like

Three defects drop exactly one line and add exactly one diagnostic naming its line number:

| Defect | Example |
| --- | --- |
| an unusable name | `/etc/passwd`, `vendor/../secret.md`, `vendor\api.md`, an empty name |
| an unusable source | `docs/local.md`, an empty value, a URL followed by a trailing remark |
| a repeated name | the second declaration of `vendor/api.md` |

A repeated name keeps its first declaration, and the entries keep the order the manifest
lists them in — that order is what the showcase renders.

### 2.4 Two trees, one direction

| Tree | Tracked by Git | Author | Holds |
| --- | --- | --- | --- |
| `.tauqe/refs/` | yes | the developer | the manifest, and any document committed beside it |
| `.tauqe/cache/refs/` | no | the harness | the snapshots it materialized |

An ignore rule never untracks a file Git already tracks, so the manifest stays committed,
reviewable material while the cache stays untracked: the declaration is reviewed, the copy is
not. The two trees are also disjoint by construction — the comparison runs over whole path
components, so a sibling whose name merely starts with the same prefix is not a registry path
and not a cache path.

### 2.5 What the model actually sees

1. While the manifest exists on disk, it is pinned read-only and rendered in the `<context>`
   block. The catalogue therefore travels with every prompt, and it costs the size of the
   manifest — tens of tokens for a project that declares a handful of documents.
2. A declared document stays invisible until a round requests it. Reading the manifest
   materializes nothing.
3. A requested document arrives at its cache path, read-only, and stays there for the rest
   of the task.

---

## 3. Mechanics & Lifecycle ("How it behaves")

### 3.1 Reading the registry

The manifest is read off the async executor thread, because reading it is file-system work.
A repository without a manifest declares no references, which is a legitimate state and
produces neither entries nor diagnostics. A manifest that exists but cannot be read is
reported as one diagnostic naming the manifest and recorded as a warning in the daemon
journal, so the degradation stays observable without aborting a turn.

The read is lazy twice over. A Discovery round reads the manifest at most once, and only when
a request actually names a path under the tracked registry directory; a round that requests
ordinary repository files never touches it.

### 3.2 The lazy pre-step of a Discovery round

A declared document has no file on disk until the harness materializes it, so the round
resolves the registry before it resolves repository facts:

1. The round collects its context requests.
2. A request naming a path under `.tauqe/refs/` is reduced to a registry name. The manifest
   itself declares the entries rather than being one, so it has no name and always travels
   unchanged.
3. A registry path that already exists on disk — a document committed beside the manifest —
   travels unchanged too. Tracked registry material is read where it is and never copied into
   the cache.
4. Otherwise the name is looked up in the manifest. A name the manifest does not declare
   stays an ordinary request and is reported missing like any other.
5. A declared entry is materialized, and the request is rewritten to its cache path with
   read-only access.
6. The round continues with the rewritten request, so the model reads the snapshot the
   registry declares instead of waiting a round for a file that would never appear.

The answers stay aligned with the requests: one resolved request per input, in the order the
round declared them, so every other command keeps its place.

### 3.3 Materialization, once

Materializing one entry follows a fixed sequence:

- the cache path is derived from the registry name, joined to the repository root, and
  verified to stay inside it;
- an existing snapshot is answered immediately, with no network request and no copy;
- a remote source is fetched over HTTP through a shared client, with a bounded connection
  budget and the default project wall-clock budget (600 seconds) per request, and the whole
  exchange — response headers and body alike — races the turn's cancellation signal, so an
  abandoned turn stops waiting for a slow host;
- a local source is read from its absolute path, off the async executor thread;
- the bytes land through the atomic replacement, parent directories included, so a reader
  observes either the previous complete snapshot or the new one and never a truncated file.

From that moment the snapshot is immutable for the whole task. That is what keeps repeated
Discovery rounds free of redundant downloads and the material the model reasoned about
stable.

### 3.4 Landing in the Refs layer

A cache path is Git-ignored, so the tracked file list never names it. Discovery admits it by
existence inside the declared cache tree, exactly as it admits a mockup, and the requested
access is coerced to read-only whatever the request asked for.

The document then enters the dedicated Refs layer:

- the file must exist, and its cost is estimated from its size like any other context entry;
- a document that alone exceeds `refs_token_budget` is refused with a diagnostic naming the
  configured budget, instead of evicting every other document and failing anyway;
- an admitted document enters read-only with its own insertion watermark, so it renders at
  the position it actually arrived in;
- a repeated request refreshes only its LRU watermark, so no rendered byte changes and the
  context revision stays put.

The layer is registry-managed. A manual addition is refused, `set_access` is refused,
`rename` and `move` are refused, and no operation promotes a snapshot to editable.

### 3.5 Eviction by recency

Admission keeps the layer inside `refs_token_budget` by evicting the least recently requested
document until the layer fits. The rule follows requests rather than arrival: a document the
dialogue keeps returning to outranks one it read once earlier, even when that one arrived
later.

### 3.6 Persistence for the whole task

The Refs layer is context, not transcript, and that single fact explains its whole lifecycle:

- history compaction rewrites history entries and never touches the layer, so a snapshot
  cannot be summarized away;
- a turn merge folds the snapshots the turn materialized into the live layer, and a path held
  on both sides keeps the more recent LRU watermark;
- pruning drops a document whose cached file disappeared;
- clearing the Auto layer leaves the layer untouched, while clearing all context drops it.

### 3.7 Reading a linked document

A snapshot may link its siblings by relative path. Because it is stored under its registry
name, such a link resolves inside `.tauqe/cache/refs/`, and a request for the linked cache
path is admitted by existence exactly like the document that named it.

A multi-file corpus is therefore declared entry by entry: each sibling that must be reachable
has its own line in the manifest, and the relative links between the snapshots tie the corpus
together without a second mechanism. A linked path that was never declared and never
materialized is reported missing like any other absent file.

### 3.8 Refreshing a snapshot

The harness never refreshes a snapshot, and no configuration flag makes it. A document that
genuinely changed at its source is refreshed out of band: delete the cache file, or the whole
cache directory, and request the document again in a later round. The harness then copies the
current bytes into a freshly materialized snapshot, and every later round of that task reads
the new one.

### 3.9 Failure semantics

- **No manifest** — nothing is declared; ordinary requests behave exactly as before.
- **An unreadable manifest** — one diagnostic naming the manifest, and the registry behaves
  as empty, so a declared document is reported missing.
- **An unusable entry** — one line dropped and one diagnostic naming the line number, while
  the remaining entries stay usable.
- **An undeclared name** — an ordinary request, reported missing like any other absent path.
- **A refused materialization** — an error status, an unreachable host, an unreadable local
  path, a timeout, or a cancelled turn stores nothing. The request stays unsatisfied, the
  round reports the file as missing, and a `<refs_feedback>` block naming the reason leads
  the round's feedback. Because nothing was stored, a later round may attempt the same entry
  again.
- **A document larger than the budget** — refused with the configured budget in the message,
  and no other document is evicted for it.

---

## 4. Boundaries & Anti-Patterns ("What it is NOT")

- **It is not a bookmark store.** The registry declares material the project reasons against,
  not links a human wants to keep. An entry earns its line by being quoted in a turn, and an
  unrequested document costs the manifest one line and nothing else.
- **It is not a cache that refreshes itself.** A snapshot never updates behind the model's
  back; refreshing is an explicit, out-of-band act (3.8). Stability is the feature.
- **It is not a document store.** The manifest declares addresses and the cache holds copies;
  neither is committed. Content that must live in the repository belongs in the repository.
- **It is not a way for a turn to choose its own sources.** This repository declares the
  manifest protected through `[project.boundaries.protected]`, so a staged edit to it is
  refused before the toolchain starts. A turn that must quote a new document reports a
  semantic blocker instead of widening its own material.
- **It is not a network client on demand.** Reading and parsing the manifest fetches nothing;
  only a request from a round fetches.
- **It is not the mockups sandbox.** The sandbox holds drafts the model writes and may
  rewrite; the cache holds material quoted from outside the repository and the model may not
  rewrite it. One directory grants authorship, the other withdraws it.
- **It is not part of the change set.** The cache is Git-ignored, so no snapshot reaches a
  commit, a post-hook capture, or the paths the verification command receives.
- **It is not a merge layer.** Two declarations of one name are not reconciled: the first
  wins and the second is reported.
- **It is not durable state the harness protects.** The cache is expendable. Deleting it
  costs one materialization per document, never a lost declaration.

---

## 5. Related Chapters

- [The Context Model](context-model.md) — the curated layers the Refs layer
  sits beside, and the access vocabulary it shares.
- [Discovery Engine](discovery-engine.md) — the round that resolves a request into a
  snapshot, and the journal that carries its findings.
- [Protected Paths and Trust Boundaries](../ch03-project/protected-paths.md) — the declaration
  that keeps the manifest out of a turn's hands.
- [The Mockups Sandbox](../ch03-project/mockups-sandbox.md) — the sibling harness-owned tree
  and the existence-based admission both trees share.
- [Git Safety & Transactions](../ch03-project/git-transactions.md) — the transaction that
  excludes `.tauqe/` from every commit.
- [Configuration and Project Preparation](../ch05-system/configuration.md) —
  `refs_token_budget` and the living template that documents it.
