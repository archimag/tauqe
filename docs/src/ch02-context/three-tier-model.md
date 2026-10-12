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
2. **User Layer:**
   - Files explicitly designated by the developer as relevant to the current work session.
   - Preserved across turns until manually cleared or promoted.
   - **Accelerating the Turn Lifecycle:** While the model can autonomously discover files, each discovery step requires an additional round-trip request. Manually adding key task-relevant files to the User layer eliminates discovery rounds, establishing an actionable context immediately and saving time and tokens.
3. **Auto Layer:**
   - Files autonomously requested by the model during multi-round Discovery via `<context_request>`.
   - Populated dynamically as the model detects missing symbols or definitions from the Tree-sitter Repo Map.
   - Can be promoted to the User layer (`p` in Context View) or cleared at will (`c`).

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
