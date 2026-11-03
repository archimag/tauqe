# External Project Documentation

Sovereign concept chapter: the `[[outer_docs]]` declaration, the isolated snapshot cache
under `.tauqe/cache/outer_docs/`, the Base Prefix Jail that governs every request below a
declared root, the OuterDocs context layer those snapshots enter, the transfer guards that
bound one snapshot, and the developer's trust declaration that decides which host material
the harness may read.

A project routinely reasons against documents it did not write: an upstream API note, a
protocol summary, a vendored `llm.txt` transcript, a published book. None of them belongs
in a commit, and none of them can be handed to the model as a URL. The declaration names
that material, the harness materializes a copy at a deterministic path inside the
repository, and a research round requests that path exactly like any other context file.

---

## 1. Motivation & Invariants ("Why")

### 1.1 External material has no address in a repository

A quoted document has two possible addresses, and neither is a repository path: an
`http://`/`https://` URL, or an absolute path on one developer's machine. Committing its
bytes would put a file nobody maintains under review forever. Pasting the whole corpus
into the prompt would spend the context budget on material the task may never read.

The remaining shortcut is worse. Letting the model fetch a URL is arbitrary network
access, refused for the same reason the harness refuses an arbitrary shell: an untyped
channel to the outside world is not a protocol operation.

The declaration replaces every shortcut. The developer writes down where the document
comes from, the harness materializes a copy at a deterministic path inside the
repository, and the model requests that path exactly like any other context file.

### 1.2 A snapshot is a promise of stability

A bounded turn reasons across several rounds: discovery, the proposal, staging retries,
and verification healing. If the document under its reasoning could change between two
rounds, the later round would argue about bytes the earlier round never saw, and neither
round would be reproducible afterwards.

The harness therefore materializes each requested document once and treats the result as
immutable. A cache file that already exists is answered byte for byte, without a transfer
and without a copy, and the harness never refreshes a snapshot on its own initiative.

### 1.3 Relative navigation must stay outside the working tree

A declared document legitimately links its siblings: `./protocol.md`,
`../vendor/api.md`. Resolving such a link against the working tree would turn quoted
external material into a way of naming any repository path.

The cache answers that. A declaration owns exactly one directory under
`.tauqe/cache/outer_docs/`: the root document lands in it under the file name its source
carries, and every relative document lands below it in the same directory. A relative
link inside a snapshot therefore resolves inside the same isolated tree, which only ever
holds snapshots.

### 1.4 The project declares intent; the developer grants authority

`tauqe.toml` travels with the repository, so it records what the project wants rather than
what the developer allowed. Public Internet material is public: a declared `http(s)` source
that resolves to a public address is fetched with no workspace entry at all, and a local
source inside the repository is the project's own material. Opening a repository therefore
cannot hand the harness a file from the developer's home directory or an answer from an
internal network.

A declaration the repository cannot reach decides the two ways a repository could still
reach past its own material: the host's file system outside the repository, and the host's
own network. That split is why the trust file is deliberately not a `tauqe.toml` section: a
repository able to authorize itself would have no boundary at all.

### 1.5 The invariants

1. **One declaration.** `[[outer_docs]]` in `tauqe.toml` or in a `tauqe.d/*.toml` layer is
   the only source of an external document. Nothing else names one, and reading the
   declaration materializes nothing.
2. **The declaration declares; it never stores.** It carries labels and one source address,
   never content.
3. **`id` is the identity of an entry.** It is a clean repository-relative path, it names
   the directory the snapshots occupy, and two entries may not share it.
4. **Exactly one source.** The single `source` key carries an `http(s)` address or a local
   path, and its scheme decides which. An entry that carries no source, or names one the
   registry cannot resolve, is a configuration defect.
5. **The harness parses no index.** The root document is answered as its own text. The
   harness never reads `SUMMARY.md`, `index.md`, `llms.txt`, or any other catalog to invent
   the documents below a root, and the model follows the links the text carries.
6. **One snapshot per cache file.** It is immutable: a cache file that exists is answered
   as is, and the harness never refreshes it, transfers it again, or rewrites it.
7. **A document is reached through one jail.** `{id}` answers the root document, and
   `{id}/{relative}` resolves below the directory that holds it. A parent directory, an
   absolute path, a backslash, an empty segment, and a foreign scheme are refused before a
   byte moves.
8. **Snapshot material is read-only.** It cannot be promoted to editable, renamed, moved,
   or re-permissioned.
9. **The cache never joins the change set.** It lives under `.tauqe/`, which every
   transaction excludes, so no snapshot reaches a commit or a verification run.
10. **Materialization happens inside the bounded turn.** The transfer races the turn's
    cancellation signal and reports a compact reason instead of panicking.
11. **The OuterDocs layer is context, not transcript.** History compaction never touches it,
    and only clearing the whole context drops it.
12. **Pricing is shared.** A document costs its content plus the wrapper its own `<file>`
    element renders, under the single `[context].token_budget` ceiling; the layer has no
    ceiling of its own and evicts nothing.
13. **A defect invalidates the declaration.** Configuration validation refuses a
    declaration carrying an unusable entry, a duplicate `id`, or a blank title, and
    reports the reason, so a turn never starts against a declaration the harness could
    not mount.
14. **Public material needs no entry.** A declared `http(s)` source that resolves to a public
    Internet address is fetched without a workspace entry, because published material is
    public. A repository therefore never has to be authorized to quote the public Internet.
15. **The trust declaration guards the host.** A local source is read without an entry while
    it lies inside the repository, which is the project's own material, and only from a
    directory the workspace declaration lists in `local_roots` once it does not. A remote
    source may reach a loopback, private, link-local, or otherwise non-public address only
    when the entry sets `allow_private_network`. A workspace without an entry reads no host
    material at all.
16. **A transfer is guarded before the body.** A declared `Content-Length` above the snapshot
    ceiling abandons the transfer before a body byte is buffered, and a non-textual
    `Content-Type` refuses the response on its headers. A body of unknown length is bounded
    by the bytes accumulated while it is read.
17. **Only text enters the cache.** Every snapshot is validated as UTF-8 before it is
    written, on the local and the remote contour alike, so a document the model could never
    read never becomes a cache file.

---

## 2. Mental Model & Ontology ("What")

### 2.1 Terms

- **Declaration** — the `[[outer_docs]]` array of tables, in `tauqe.toml` or in any
  `tauqe.d/*.toml` layer merged into it.
- **Entry** — one declared document: an `id`, a `title`, an optional `description`, and
  exactly one `source`.
- **`id` (mount point)** — the clean repository-relative path that identifies an entry and
  names the directory its snapshots occupy.
- **Source** — the single `source` string, typed by its scheme: an `http://`/`https://`
  address is fetched over the network, and every other value names a local file.
- **Snapshot** — the bytes the harness materialized from a source.
- **Root document** — the document the entry's `source` names, cached under the file name
  that source carries.
- **Relative document** — a document below the directory that holds the root, cached under
  the relative path a link names.
- **Base Prefix Jail** — the addressing rule that every request names the root document or a
  clean relative path below it, and nothing else.
- **Cache path** — the repository-relative path of a snapshot:
  `.tauqe/cache/outer_docs/<id>/<file name>`.
- **OuterDocs layer** — the context layer holding snapshots read-only for the whole task.
- **Trust declaration** — the developer's `refs-trust.toml`, written outside the
  repository.
- **Trust refusal** — a typed decision of that declaration, addressed to the developer.
- **Delivery failure** — everything else that kept a snapshot from being materialized,
  addressed to the model.

### 2.2 The declaration

```toml
[[outer_docs]]
id = "vendor/api.md"
title = "Upstream API notes"
description = "Endpoint contracts the implementation step is checked against."
source = "https://example.com/llm.txt"

[[outer_docs]]
id = "vendor/protocol.md"
title = "Protocol summary"
source = "/srv/docs/protocol.md"

[[outer_docs]]
id = "mylib"
title = "MyLib Book"
source = "https://example.com/book/index.md"
```

A project that declares nothing mounts nothing. Because arrays of tables concatenate
across the modular layers, a project may split its entries across `tauqe.d/*.toml` files
and keep `tauqe.toml` for the harness policy alone.

### 2.3 One source, two shapes

| `source` value | Typed as | Mounts |
| --- | --- | --- |
| an `http://`/`https://` address with a host, carrying no whitespace and no control character | a remote source | one document fetched over the network |
| any other non-empty string | a local source | one file read from its absolute path |

`source` is the only key that names content, and it names exactly one document. Not none:
`source` is a required key, so an entry that omits it fails to decode, and an empty value is
refused while the configuration is validated. Not several: there is no second key, so an
entry cannot name two documents at once and the harness never has to resolve an ambiguity by
a fixed precedence.

The scheme decides the kind. An `http(s)` address must carry a host, and a value carrying a
scheme the registry does not fetch — `file://`, `ftp://` — is refused while the configuration
is validated instead of being retried as a local path.

### 2.4 `id` is the mount point

| Declaration | Requested as | Cached at | Bytes read |
| --- | --- | --- | --- |
| `id = "vendor/api.md"` with `source = "https://example.com/llm.txt"` | `vendor/api.md` | `.tauqe/cache/outer_docs/vendor/api.md/llm.txt` | the whole document |
| `id = "mylib"` with `source = "https://example.com/book/index.md"` | `mylib` | `.tauqe/cache/outer_docs/mylib/index.md` | the whole root document |
| the same entry | `mylib/architecture.md` | `.tauqe/cache/outer_docs/mylib/architecture.md` | the relative document the link names |

The root file keeps the file name its source carries, and a source that names no usable file
name caches under `index.md`, so the layout stays total instead of failing a request the
trust declaration could still authorize.

An `id` is a clean repository-relative path written with `/`: non-empty, never absolute,
carrying no `.` or `..` segment, no backslash, no NUL, and no `:` (which would smuggle a
URL scheme or a drive letter). It is unique across the declaration, because a duplicate
would mount two documents at one path.

A document below the root is requested by the identifier `{id}/{relative}`, whatever the
link inside the root text looks like. `{id}/{relative}` is held to the same cleanliness: an
empty segment, a `.` or `..` segment, a backslash, a foreign scheme, and an absolute path
all leave the declared root and are refused before a byte moves.

### 2.5 Two trees, one direction

| Tree | Versioned by Git | Author | Holds |
| --- | --- | --- | --- |
| `tauqe.toml`, `tauqe.d/*.toml` | yes | the developer | the declaration |
| `.tauqe/cache/outer_docs/` | no | the harness | the snapshots it materialized |

Nothing inside `.tauqe/` is versioned, so the directory is ignored as a whole and no path
in it can be staged, committed, or handed to a verification command. The two trees are
also disjoint by construction: the cache comparison runs over whole path components, so a
sibling whose name merely starts with the same prefix is neither a cache path nor a
snapshot.

### 2.6 What the model actually sees

1. A declared document stays invisible until a round requests it. Reading the declaration
   materializes nothing.
2. A requested document arrives at its cache path, `read_only`, and stays in the OuterDocs
   layer for the rest of the task.
3. A request for write access is coerced to `read_only`: the layer never promotes a
   snapshot to editable, because material quoted from outside the repository must not be
   rewritten by the turn that reasons about it.

An outer-document request names the mount point — the `id` of an entry — so where the
materialized copy is cached stays an implementation detail of the harness. A request for
`{id}` answers the root document as its original text, and a request for `{id}/{relative}`
answers the document below it. Not one cache path appears in that answer: a root that
carries `- [Architecture](architecture.md)` is answered with its own text, and the model
asks for `mylib/architecture.md` next. A context request naming a path under
`.tauqe/cache/outer_docs/` reaches the same snapshots directly, which keeps a round that
already knows a cache path able to ask for it.

The declaration is nevertheless published. The system prompt carries a compact catalog of
the declared documents — one line per entry, naming its `id`, its `title`, and its
optional `description` — so the model knows which documents exist and may name one in a
request. The catalog never carries a source address, a host path, or a cache path, and it
is rendered only for a turn that can actually request an external document; reading it
materializes nothing, so the material itself still arrives only when a round requests it.

### 2.7 The developer's trust declaration

The declaration states what a project wants. Which host material the harness may touch is
decided by a file the repository cannot reach: `$XDG_CONFIG_HOME/tauqe/refs-trust.toml`, or
`~/.config/tauqe/refs-trust.toml` when that variable is unset. One entry authorizes one
workspace:

```toml
[[workspace]]
root = "/home/dev/work/project"
local_roots = ["/srv/docs/upstream"]
allow_private_network = false
```

- `root` is the absolute path of the repository the entry authorizes. A repository without
  a matching entry reaches no host material, which is the default: its declared public
  addresses are still fetched and its own files are still read.
- `local_roots` lists the absolute directories a declared local source may be read from.
  The comparison runs over canonical paths, so a symlink inside a root cannot carry the
  read outside it.
- `allow_private_network` lets a declared remote source address a loopback, private,
  link-local, or otherwise non-public network. It defaults to `false`, because a host
  reachable only from inside a network is exactly what a repository nobody reviewed would
  address.
- `local_roots` lists the absolute directories a declared local source may be read from.
  The comparison runs over canonical paths, so a symlink inside a root cannot carry the
  read outside it.
- `allow_private_network` lets a declared remote source address a loopback, private,
  link-local, or otherwise non-public network. It defaults to `false`, because a host
  reachable only from inside a network is exactly what a repository nobody reviewed would
  address.

The file keeps its historical `refs-trust.toml` name. It is the developer's own
declaration, written once outside the repository, and renaming it would silently revoke a
trust that was already granted.

### 2.8 What the harness refuses

A declaration defect is answered by configuration validation:

| Defect | Effect |
| --- | --- |
| an `id` that is not a clean repository-relative path | the declaration is refused, naming the id |
| two entries that share an `id` | the declaration is refused, naming the id |
| an empty `title` | the declaration is refused, naming the entry |
| an entry without a `source` | the declaration fails to decode: `source` is a required key |
| an empty `source` | the declaration is refused, naming the entry |
| a source carrying a scheme the harness does not fetch | the declaration is refused, naming the key and its value |
| an `http(s)` address without a host | the declaration is refused, naming the source |

A declared source that reaches host material the developer's declaration does not cover is
answered by a typed refusal:

| Refusal | Cause | Remedy carried |
| --- | --- | --- |
| `WorkspaceNotAuthorized` | no `[[workspace]]` entry names this repository root, so the host stays unreachable | the entry that authorizes the workspace |
| `LocalSourceOutsideRoots` | the canonical source lies outside every declared local root | the entry extended with the source's own directory |
| `CredentialsInUrl` | the address carries credentials the harness would hand to its host | the entry, plus the note that the address must be stripped |
| `NonPublicNetwork` | the source resolves to a non-public address | the entry with `allow_private_network = true` |
| `UnsupportedTarget` | a redirect hop reaches an address that is not an `http(s)` URL with a host | none: the declaration must name a different source |

A remote target is constrained by address rather than by name. Loopback, private,
link-local, broadcast, documentation, unspecified, and multicast IPv4 blocks, `0.0.0.0/8`,
the carrier-grade NAT block `100.64.0.0/10`, and the reserved `240.0.0.0/4` are non-public,
as are the IPv6 unique-local `fc00::/7`, link-local `fe80::/10`, documentation
`2001:db8::/32`, the NAT64 prefix `64:ff9b::/96`, and an IPv4-mapped address that reaches
a non-public IPv4 target.

Everything else is a delivery failure, and the two kinds are never merged. A trust refusal
addresses the developer: no retry inside a turn can widen a declaration the repository
cannot reach, so the model is told to report the missing document instead of routing
around the prohibition. A delivery failure addresses the model: the source may be
unreachable now and reachable later, so the model works with the material it already holds
and may request the same document in a later round.

---

## 3. Mechanics & Lifecycle ("How it behaves")

### 3.1 Reading the declaration

The configuration is decoded from the merged document — the base `tauqe.toml` and the
`tauqe.d/*.toml` layers — and each entry resolves its single `source` at that moment, so a
defect is reported while the configuration is validated rather than in the middle of a
turn. The effect is deliberately blunt: one unusable entry invalidates the declaration as
a whole, and the project configuration is reported as a diagnostic instead of being
partially honored. A declaration that loads is a declaration the harness could mount.

At runtime the registry is built from those resolved entries, and an entry that still
cannot be resolved is reported once and left out, so one broken declaration never hides
the documents around it. That degradation is a second line of defense: validation has
already rejected such an entry before the turn started.

### 3.2 The lazy pre-step of a research round

A declared document has no file on disk until the harness materializes it, so a round
resolves the declaration before it resolves repository facts:

1. The round collects its context requests and the outer-document requests that name a mount
   point of the declaration.
2. An outer-document request materializes the document it names: `{id}` materializes the root
   document, and `{id}/{relative}` materializes the document below it. The resulting cache
   path joins the round as one read-only context request.
3. A context request naming a path under `.tauqe/cache/outer_docs/` is reduced to the entry it
   addresses and materialized in the same way.
4. A path the declaration does not address stays an ordinary request and is reported
   missing unless the snapshot already exists, exactly as any absent file is.
5. A refusal is recorded with its audience: a trust decision becomes a typed entry of the
   round outcome and a feedback block naming the exact cause and the declaration that
   would authorize the source, while a delivery failure becomes a feedback block naming
   the reason and the invitation to retry later.

The answers stay aligned with the requests: one resolved request per input, in the order
the round declared them, so every other command keeps its place.

### 3.3 Materialization, once

Materializing one document follows a fixed sequence:

- the cache path is derived from the `id` and the requested document, joined to the
  repository root, and verified to stay inside it;
- an existing snapshot is answered immediately, with no transfer and no copy, and before
  the trust rule is consulted: the material is already local, and no later decision revokes
  what an earlier round delivered;
- a local source inside the repository is the project's own material and is read without an
  entry, while a source outside it is read only from a directory the workspace declaration
  allows, resolved through its canonical path so a symlink cannot carry the read outside
  the root;
- a remote source is resolved by the harness itself, and every address it reaches must be
  public unless the declaration allows a non-public network. A public address is fetched
  without an entry. The validated address is pinned into the request, the URL may carry no
  credentials, and the harness follows redirects hop by hop through the same check instead
  of letting the HTTP client follow them;
- the transfer runs with a 30-second connection budget and the default project wall-clock
  budget (600 seconds) per request, and the whole exchange — response headers and body
  alike — races the turn's cancellation signal, so an abandoned turn stops waiting for a
  slow host;
- the response is guarded on its headers before the body is read: a declared
  `Content-Length` above the snapshot ceiling abandons the transfer, and a media type
  outside the textual family (`text/*`, `application/json`, `application/xml`,
  `application/yaml`) refuses it as binary material;
- the size of one snapshot is bounded: a document larger than 4 MiB is refused with a
  compact reason instead of being buffered and rejected by the token budget afterwards, and
  a body that declares no length is bounded while it is read;
- the bytes are validated as UTF-8 text and land through the atomic replacement, parent
  directories included, so a reader observes either the previous complete snapshot or the
  new one and never a truncated file.

From that moment the snapshot is immutable for the lifetime of the cache file. That is
what keeps repeated research rounds free of redundant transfers and the material the model
reasoned about stable.

### 3.4 Navigating by links

The harness parses no index. A root document that carries
`- [Architecture](architecture.md)` is answered as its own text, and the model requests
`mylib/architecture.md` in a later round. Placement follows that addressing rule:

- the root document lands under the file name its source carries:
  `https://example.com/book/index.md` with `id = "mylib"` becomes
  `.tauqe/cache/outer_docs/mylib/index.md`;
- a relative document lands below it under the relative path the link names: `mylib` plus
  `architecture.md` becomes `.tauqe/cache/outer_docs/mylib/architecture.md`;
- a remote relative document is joined below the root with a trailing separator, so a
  relative reference extends the root instead of replacing its last segment;
- documents are materialized lazily, one per request, so a corpus that links forty files
  transfers only the ones the turn actually reads;
- a link that leaves the declared root — an external address, an absolute path, a `..`
  segment — costs that request: it is refused rather than followed.

Because a link resolves inside the directory of the declaration, a sibling the root links
to needs no declaration of its own: authorizing the root authorizes the subtree the jail
keeps inside it. A path that is neither declared nor materialized is reported missing like
any other absent file.

### 3.5 Landing in the OuterDocs layer

A cache path is Git-ignored, so the tracked file list never names it. Research admits it by
existence inside the declared cache tree, exactly as it admits a mockup, and the requested
access is coerced to read-only whatever the request asked for.

The document then enters the dedicated OuterDocs layer:

- the file must exist, and its cost is estimated from its size like any other context
  entry: its content plus the wrapper its own `<file>` element renders;
- it competes with the repository files for the single `[context].token_budget` ceiling
  (300,000 tokens by default). A request that would breach that ceiling is refused whole,
  naming every path it asked for, and the snapshot stays in the cache for a later round;
- the layer applies no ceiling of its own, keeps every admitted document, and never evicts
  by recency;
- a repeated request changes nothing: no rendered byte changes, and the context revision
  stays put.

The layer is registry-managed. A manual addition is refused, `set_access` is refused,
`rename` and `move` are refused, and no operation promotes a snapshot to editable.

A plan step declares the external documents its implementation is checked against through
the `outer_docs` field of its leaf, and the harness materializes and mounts them before the
step turn starts (*Local Plans*). The layer is the same one a research round widens, so a
step that names its documents spends no research round discovering them.

### 3.6 Persistence for the whole task

The OuterDocs layer is context rather than transcript, and that single fact explains its
whole lifecycle:

- history compaction rewrites history entries and never touches the layer, so a snapshot
  cannot be summarized away;
- sweeping the Auto layer at the end of a turn leaves the layer untouched, so a document
  outlives the round that requested it;
- a turn merge folds the documents the turn materialized into the live layer;
- pruning drops a document whose cached file disappeared;
- clearing the Auto layer leaves the layer untouched, while clearing all context drops it.

### 3.7 Refreshing a snapshot

The harness never refreshes a snapshot, and no configuration flag makes it. A document
that genuinely changed at its source is refreshed out of band: delete the directory of its
declaration — the root file and the relative documents beside it — and request the document
again in a later round. The harness then copies the current bytes into a freshly
materialized snapshot, and every later round reads the new one. Stability is the feature,
so refreshing is an explicit act rather than a background policy.

### 3.8 Failure semantics

- **No declaration** — nothing is mounted; ordinary requests behave exactly as before.
- **A declaration carrying a defect** — configuration validation names the entry and the
  reason, and the project configuration is reported instead of being partially honored.
- **An entry the runtime registry cannot resolve** — one diagnostic, the entry is left out,
  and the documents around it stay mounted.
- **An undeclared cache path** — an ordinary request, reported missing unless a snapshot
  from an earlier task already occupies it.
- **A request that leaves the declared root** — refused before a transfer, naming the root
  and the rule it enforces.
- **A refused materialization** — an error status, an unreachable host, an unreadable local
  path, a timeout, or a cancelled turn stores nothing. The request stays unsatisfied, a
  feedback block naming the reason leads the round's feedback, and a later round may
  attempt the same document again because nothing was stored.
- **A response that declares too large a body** — a `Content-Length` above the snapshot
  ceiling abandons the transfer before the body is read, with the ceiling in the message,
  and no other document is displaced for it.
- **A body larger than the byte ceiling** — a stream that declares no length is abandoned
  the moment the accumulated bytes cross the same ceiling.
- **A response that is not documentation text** — a media type outside the textual family is
  refused from its headers, as is a body that is not valid UTF-8 before the atomic write,
  so binary material never becomes a cache file.
- **A workspace the declaration does not authorize** — every host source is refused with the
  reason, so nothing outside the repository and no non-public address is read; a public
  address is still fetched and the repository's own files are still read.
- **A local source outside every declared root** — refused by canonical path, so a symlink
  does not widen the root. The trust file belongs to the developer, so a turn cannot widen
  it either: a turn that needs another directory reports a blocker.
- **A remote target that is not public** — refused before a connection is opened, naming the
  address. The same rule applies to every redirect hop, to a URL that carries credentials,
  and to a host that resolves to a non-public address.
- **An endless redirect chain** — the harness follows at most five hops and then abandons
  the transfer with a reason.
- **A step whose declared document is refused by the trust declaration** — the step is never
  started, and the run stops with the exact cause and the `[[workspace]]` block that would
  authorize the source (*Local Plans*).
- **A step whose declared document cannot be delivered** — the step starts with the reason
  in its prompt, and the model routes around the missing material.

---

## 4. Boundaries & Anti-Patterns ("What it is NOT")

- **It is not a bookmark store.** The declaration names material the project reasons
  against, not links a human wants to keep. An entry earns its line by being quoted in a
  turn, and an unrequested document costs one declaration and nothing else.
- **It is not a cache that refreshes itself.** A snapshot never updates behind the model's
  back; refreshing is an explicit, out-of-band act (3.7).
- **It is not a document store.** The declaration carries an address and the cache holds
  copies; neither is versioned. Content that must live in the repository belongs in the
  repository.
- **It is not an index parser.** The root document is answered as text. The harness never
  reads a `SUMMARY.md`, an `index.md`, or an `llms.txt` to reconstruct a table of contents,
  and a document is requested one link at a time.
- **It is not a way for a turn to choose its own sources.** The declaration is project
  configuration, and this repository protects `tauqe.d/**` through
  `[project.boundaries.protected]`, so a staged edit to a module is refused before the
  toolchain starts. A turn that must quote a new document reports a semantic blocker
  instead of widening its own material.
- **It is not authorized by the repository it travels with.** The declaration records
  intent; which directories may be read and whether a non-public network may be addressed
  are decided by a file the repository cannot reach.
- **It is not a network client on demand.** Reading the declaration fetches nothing; only a
  request from a round materializes a source.
- **It is not the mockups sandbox.** The sandbox holds drafts the model writes and may
  rewrite; the cache holds material quoted from outside the repository and the model may
  not rewrite it. One directory grants authorship, the other withdraws it.
- **It is not part of the change set.** The cache is Git-ignored, so no snapshot reaches a
  commit, a post-hook capture, or the paths the verification command receives.
- **It is not a merge layer.** Two entries that share an `id` are refused, not reconciled:
  the harness never guesses which of two documents should answer a request.
- **It is not a per-layer budget.** There is no separate external-document ceiling, no
  eviction order, and no recency ranking: a document costs what every other context entry
  costs and lives until the context is cleared.
- **It is not durable state the harness protects.** The cache is expendable. Deleting it
  costs one materialization per document, never a lost declaration.
- **It is not a substitute for the trust declaration.** A declaration a repository writes
  cannot reach the host's file system or the host's own network, and a trust refusal is
  never retried into success by a later round of the same turn.
- **It is not an unguarded download.** A public address is fetched without an entry, but the
  transfer is still bounded by the snapshot ceiling and still refuses a non-textual or
  non-UTF-8 body before it reaches the cache.
- **It is not a research-only channel.** A plan step declares the documents it obeys, and
  the harness mounts them before the step turn starts; the layer is the same one a research
  round widens.

---

## 5. Related Chapters

- [The Context Model](context-model.md) — the curated layers the OuterDocs layer sits
  beside, and the access vocabulary it shares.
- [Research Engine](research-engine.md) — the round that resolves a request into a
  snapshot, and the journal that carries its findings.
- [Protected Paths and Trust Boundaries](../ch03-project/protected-paths.md) — the
  declaration that keeps a configuration layer out of a turn's hands.
- [The Mockups Sandbox](../ch03-project/mockups-sandbox.md) — the sibling harness-owned
  tree and the existence-based admission both trees share.
- [Git Safety & Transactions](../ch03-project/git-transactions.md) — the transaction that
  excludes `.tauqe/` from every commit.
- [Configuration and Project Preparation](../ch05-system/configuration.md) — the modular
  layers, `[context].token_budget`, and the living template that documents them.
