# 2. Three-Tier Context Model

TAUQE eliminates manual file micromanagement through a structured three-tier context hierarchy and autonomous file discovery, while preserving strict context hygiene.

---

## 2.1 Purpose of Context and Context Hygiene

The assembled context serves as the active working memory provided to the model during each turn. It contains the exact instructions, conventions, and source files the model requires to understand and modify the codebase.

### The Pitfall of Context Bloat
In large, real-world repositories:
- **Impracticality and Token Limits:** Sending the entire codebase into the model's context is impractical or physically impossible due to token window constraints and budget limits.
- **Attentional Degradation:** Feeding extraneous files that are not directly relevant to the current task degrades model performance. Unrelated code introduces noise, dilutes attention ("needle in a haystack" effect), increases latency and cost, and frequently leads to hallucinations or degraded patch generation.
- **Minimal Sufficient Context:** Effective engineering assistance relies on providing the minimal sufficient set of files needed to reason about and solve the problem cleanly.

---

## 2.2 Context Layers

The working context is organized into three distinct layers:

1. **Pinned Layer:**
   - Permanent project guidelines and index files configured in `tauqe.toml` (such as `Conventions.md`, `Vision.md`, and `docs/src/SUMMARY.md`).
   - Injected into every prompt to maintain continuous adherence to project standards.
   - **Strictly protected:** Pinned files are read-only by default and cannot be removed or renamed from the interface. Only an explicit change to project configuration can alter this layer.
2. **User Layer:**
   - Files explicitly designated by the developer as relevant to the current work session.
   - Preserved across turns until manually cleared or promoted.
   - **Precedence:** A file added to the User layer is promoted out of the Auto layer. Because `editable` access always wins over `read_only` across every layer, curating a file as editable in the User layer upgrades its effective permission even when it is also pinned.
   - **Accelerating the Turn Lifecycle:** While the model can autonomously discover files, each discovery step requires an additional round-trip request. Manually adding key task-relevant files to the User layer eliminates discovery rounds, establishing an actionable context immediately and saving time and tokens.
3. **Auto Layer:**
   - Files autonomously requested by the model during multi-round Discovery via `<context_request>`.
   - Populated dynamically as the model detects missing symbols or definitions from the Tree-sitter Repo Map.
   - **Disposable and evolving:** Files here can be promoted to the User layer (`p` in Context View), cleared in bulk (`c`), or retired individually via `<context_drop path="..." />` under the active discovery mode (see 2.5). The Auto layer is the model's working set, not a durable commitment.

---

## 2.3 Permissions: Knowledge Scope vs Write Scope

Every context item carries semantic permission boundaries:
- **`read_only` (Knowledge Scope):** Accessible for reading, structural comprehension, and referencing definitions. Files in this scope serve as a semantic hint to the model that they are primarily intended for reference. However, if fulfilling a task genuinely requires updating a file present in the read-only scope, TAUQE allows the modification and automatically promotes the file to the `editable` scope upon applying the patch. This eliminates unnecessary round-trips and token waste while keeping the developer fully informed via live streaming diffs.
- **`editable` (Write Scope):** Explicitly designated for modification.

Files not present in the active context (visible only in the Tree-sitter Repo Map) remain strictly protected: the model cannot edit them blindly and must first request them into context via `<context_request>`.

---

## 2.4 Autonomous Discovery and Synergy with User Context

TAUQE balances developer control with model autonomy:
1. When a task is submitted, the model inspects the Tree-sitter Repo Map alongside the currently loaded files.
2. If symbols or implementations are missing from context, the model emits protocol requests (e.g., `<context_request path="crates/core/src/context.rs" access="read_only" />`). Files already present in context (including read-only files) do not require a separate discovery round and may be edited directly if required.
3. The server adds requested files to the `Auto` context layer and immediately initiates the next round.
4. Once all needed files are loaded, the model produces code modifications.

By combining explicit User layer curation with autonomous model Discovery, developers can provide the high-level focus while allowing the harness to fill in detailed dependencies on demand.

---

## 2.5 Discovery Modes: Context Growth Strategies

How the Auto layer evolves across discovery rounds is governed by a configurable strategy (`context.discovery_mode` in `tauqe.toml`). Each mode encodes a different trade-off between predictability and context economy.

- **`monotonic` (default):**
  - Pure append-only growth. Requested files accumulate until the turn ends; neither eviction nor `<context_drop>` removes anything.
  - *Why:* The most conservative guarantee. Once the model has reasoned about a file, it keeps it for the rest of the turn, so no dependency can silently vanish mid-thought. The cost is a steadily growing context, which is acceptable for short, well-scoped turns.
- **`incremental`:**
  - Additive expansion with explicit eviction. The model may retire an investigated dead end by emitting `<context_drop path="..." />`.
  - Only the Auto layer is affected. Attempts to drop a User or Pinned file are safely ignored, so the developer's explicit curation is never lost.
  - *Why:* Lets the model prune branches it has ruled out and reinvest the freed token budget into productive lines of inquiry, without discarding the developer's committed context.
- **`snapshot`:**
  - The model declares the full desired working set on every round. Any Auto file omitted from the current round is evicted automatically.
  - User and Pinned files are strictly preserved and never evicted.
  - *Why:* Drives toward a precise, minimal working set each round. Powerful when the model can reliably restate its complete dependency list, but demanding: silently forgetting a still-needed file evicts it.

In every mode the hard `max_files` limit is evaluated *after* eviction and reconciliation, so a snapshot that frees stale files can admit new ones within the same budget.

---

## 2.6 Glob Patterns and Token Budgets

**Batch glob intake.** Besides individual `<context_request>` tags, a whole family of files can be loaded by glob pattern or directory prefix (for example, `crates/core/src/*.rs`). The intake routine matches against the repository file list, skips files already curated in the User layer, sorts matches for deterministic ordering, and applies a safety cap of 150 files per batch. The cap prevents a single broad pattern from silently flooding the context window.

**Token budgets.** TAUQE never guesses what it can measure deterministically:
- Per-file cost is estimated from file size (roughly `size_bytes / 4`), and the assembled context reports an aggregate `total_estimated_tokens`.
- `context.max_files` bounds the number of tracked files (default 25).
- `context.max_discovery_rounds` bounds discovery round-trips per turn (default 5), preventing unbounded exploration loops.
- `context.repomap_token_budget` bounds the Tree-sitter Repo Map, keeping the architectural overview cheap.

Every budget is configurable through `tauqe.toml`; no operational limit is hardcoded.

---

## 2.7 Auto-Layer Cleanup on Cancel and Rollback

The Auto layer is ephemeral by design: it reflects the model's current investigation and must never leak stale or nonexistent files into later turns.

- **Explicit clearing (`c`):** The developer can clear the entire Auto layer, leaving a context composed solely of Pinned and User files.
- **Promotion is durable; discovery is not:** Promoting an Auto file to the User layer (`p`) turns it into a deliberate, surviving choice. Everything left behind in the Auto layer stays disposable.
- **Turn merge:** When a turn completes, its Auto-layer state is merged back into the live context. Concurrent User-layer additions made while the turn was running are preserved, and the Auto layer is replaced by the turn's final state.
- **Disk reconciliation:** After an apply, undo, or rollback, the context is pruned against the filesystem. Files that the model created and a rollback then removed no longer exist and are dropped from context automatically. This keeps the working set aligned with what Git and the filesystem actually contain, rather than with a stale in-memory snapshot.
