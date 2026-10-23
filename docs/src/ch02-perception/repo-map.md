# Repository Map and Blinkers

The repository map is the model's view of the codebase: symbol contours extracted
deterministically from tracked files and compressed into a fixed token budget. It
is the blinkers of the harness — enough orientation to navigate, never enough to
mistake a contour for a contract.

---

## 1. Motivation & Invariants ("Why")

### 1.1 Orientation without a repository

A model that cannot see what exists cannot ask for the right file. Attaching the
whole repository is impossible; attaching nothing leaves discovery blind, because
the model does not know which paths are worth requesting. The map fills that gap:
it names the files and the declarations inside them, so the next request is aimed
instead of guessed.

### 1.2 Deterministic contours, never summaries

A generated summary of a codebase is unauditable: it reads plausibly and cannot be
verified. The map is produced by Tree-sitter from the Git-tracked file list, so the
same tree always produces the same outline, and every line corresponds to a real
declaration.

### 1.3 The invariants

1. **Generated, not narrated.** Symbols come from an AST parser, never from a
   model.
2. **Contours, not contracts.** The map carries names, kinds, and shapes. Exact
   field layouts, trait bounds, lifetimes, error types, and invariants must be
   read from source before a step depends on them.
3. **Budgeted.** Every outline fits `context.repomap_token_budget`, or is
   truncated deterministically.
4. **Stable within a turn.** The map is generated once for a turn and frozen;
   context additions never regenerate it.
5. **Links are classified, never followed.** A tracked symbolic link is inspected
   with `lstat`, not dereferenced.
6. **One outline at a time.** The prompt renders either the eager symbol map or
   the compact directory tree, never both.

---

## 2. Mental Model & Ontology ("What")

### 2.1 Terms

- **Repository map (symbol map)** — the ordered outline of files and declarations.
- **Contour** — one declaration line: a path header, a symbol header, or a member
  line. A contour says a declaration exists and roughly what it looks like.
- **Blinkers** — the property that the model sees contours instead of full source:
  it can navigate, but it cannot claim to know a contract.
- **File block** — a path header with its line count followed by indented symbols.
- **Eager map** — the global symbol map rendered on every turn when
  `context.include_repo_map = true`.
- **Compact directory outline** — the `<repo_dirs>` folder tree rendered instead of
  the eager map when the eager map is disabled.
- **Targeted outline** — the symbol map of one requested subtree, rendered during
  discovery.

### 2.2 Symbols

The outline distinguishes structs, enums, traits, functions, methods,
implementations, and modules. Each symbol carries a name, a kind, a shape, a
visibility flag, a line, and its children. A file block carries the repository
path and the file's line count.

### 2.3 Compression levels

| Level | What is rendered |
| --- | --- |
| `Full` | public and private symbols, members, enum variants |
| `PublicWithDetails` | public symbols and their details |
| `OutlinesOnly` | public declaration headers, methods of trait implementations |
| `TopLevelOnly` | declaration headers only |

The levels are a ladder. The renderer starts at `Full` and steps down until the
outline fits the budget.

### 2.4 Symbolic links

Git tracks links as ordinary entries. The map classifies every path before it is
parsed:

| Status | Meaning | Rendering |
| --- | --- | --- |
| `Internal` | resolves to an existing path inside the repository | `alias.rs -> src/lib.rs` |
| `OutsideRepository` | resolves outside the repository root | `escape.rs -> /etc/passwd (outside repository)` |
| `Broken` | the target does not exist | `dangling.rs -> missing.rs (broken)` |

---

## 3. Mechanics & Lifecycle ("How it behaves")

### 3.1 Generation pipeline

1. Read the tracked file list.
2. Classify every path with `lstat`.
3. Drop every file already present in the active context: those files travel
   verbatim, so repeating their symbols would duplicate tokens.
4. Send regular files of supported languages to the Tree-sitter extractor.
5. Keep every remaining regular file as a plain path entry, so unsupported
   languages and data files still appear in the outline.
6. Sort and deduplicate the tracked entries.
7. Format symbols and plain paths within the token budget.

### 3.2 Compression and the budget ladder

Each file renders as `path (N lines)` followed by its symbols, indented two spaces
per nesting level. As the level drops, function signatures collapse to
`pub fn name` or `fn name`, enum variants keep their names without payloads,
non-public members disappear, and implementations lose private helpers while
keeping trait methods. Headers are cut at the opening brace or the trailing
semicolon, so a declaration occupies one line regardless of its body.

The renderer does not guess which level fits. It formats the whole map at `Full`,
then `PublicWithDetails`, `OutlinesOnly`, and `TopLevelOnly`, and returns the first
result inside the budget. If even the last level overshoots, it greedily includes
whole file blocks and then plain paths until the next entry would exceed the
budget. The returned outline therefore never overshoots its budget silently.

### 3.3 Symbolic links

A link is never parsed. An internal link, if parsed, would duplicate the target's
declarations; an escaping link would pull external content into the prompt. The map
therefore renders the link path and its stored target with a status suffix, and
only regular files reach the extractor.

### 3.4 Targeted outline

When the eager map is disabled, or when a subsystem is too deep to read from the
global outline, discovery requests a targeted outline with
`<repo_map path="crates/core" />`.

- The path is a repository-relative prefix matched on whole components, so
  `crates/core` covers `crates/core/src/lib.rs` but never `crates/core-utils/`.
- Naming a single file narrows the outline to that file; an empty path names the
  whole repository.
- The targeted map uses the same budget and degrades through the same ladder.
- Files already in context are not excluded: the request exists precisely to read
  the symbols of the subsystem under inspection.
- An unknown prefix yields an empty outline with an explanatory block instead of a
  repository-wide map.
- At most three distinct targeted outlines are honored per discovery round.

### 3.5 Eager map versus compact directory outline

`context.include_repo_map` selects the outline:

- `true` (default) renders the eager symbol map on every turn within
  `context.repomap_token_budget` (4,000 tokens by default).
- `false` removes the eager map and renders `<repo_dirs>` instead: a folder tree
  derived from the tracked file list, indented two spaces per level, with service
  directories filtered out.

The compact tree deliberately hides nested files but lists two categories
explicitly. Repository root files are named, because a model that cannot see
`Cargo.toml`, `tauqe.toml`, or `README.md` cannot reason about the project. Pinned
files carry a `[pinned]` suffix, so the model knows authoritative conventions
already travelled with the prompt.

The two outlines are mutually exclusive by design. The prompt renders the symbol
map when it has one and the directory tree only when it does not, so the model
reads code contours or folder layout, never both.

### 3.6 Stability across a turn

The eager map is generated once for a turn and frozen. Regenerating it after
discovery would drop the files discovery just added and rewrite thousands of
cached prompt tokens, so the map is part of the stable prefix. Targeted outlines
requested during discovery are round feedback, not a new map.

---

## 4. Boundaries & Anti-Patterns ("What it is NOT")

- **It is not source code.** A contour cannot be used as a contract; interfaces
  must be read before a step relies on them.
- **It is not a summary.** Nothing in the map is generated by a model.
- **It is not a search engine.** Locating a symbol's uses is a repository search.
- **It is not a file listing.** A directory listing answers which files exist; the
  compact outline answers where subsystems live.
- **It is not a language server.** Only supported languages produce symbols; every
  other tracked file appears as a path entry.
- **It is not recursive through links.** A symbolic link is classified and
  rendered, never traversed.
- **It is not unbounded.** Every outline is clipped deterministically to its
  budget.
- **It is not both outlines.** The prompt never carries the symbol map and the
  folder tree at the same time.

---

## 5. Related Chapters

- [Three-Tier Context Model](three-tier-model.md) — the working set the map
  complements and the context files the map excludes.
- [Discovery Engine](discovery-engine.md) — where targeted outlines are requested
  and bounded.
- [The Autonomous Bounded Turn FSM](../ch01-foundations/bounded-turn-fsm.md) —
  how the outline stays byte-stable across rounds.
