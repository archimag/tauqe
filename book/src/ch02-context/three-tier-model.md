# 2. Three-Tier Context Model

TAUQE eliminates manual file micromanagement through a structured three-tier context hierarchy and autonomous file discovery.

---

## 2.1 Context Layers

The working context consists of three distinct layers:

1. **Pinned Layer:**
   - Permanent project guidelines and index files configured in `tauqe.toml` (such as `docs/Conventions.md`, `docs/Vision.md`, and `book/src/SUMMARY.md`).
   - Injected into every prompt to maintain continuous adherence to project standards.
2. **User Layer:**
   - Files explicitly designated by the developer as relevant to the current work session.
   - Preserved across turns until manually cleared or promoted.
3. **Auto Layer:**
   - Files autonomously requested by the model during multi-round Discovery via `<context_request>`.
   - Populated dynamically as the model discovers missing definitions from the Repo Map.
   - Can be promoted to the User layer (`p` in Context View) or cleared at will (`c`).

---

## 2.2 Permissions: Knowledge Scope vs Write Scope

Every context item carries strict permission boundaries:
- **`read_only` (Knowledge Scope):** Accessible for reading, structural comprehension, and referencing definitions. Edits proposed to read-only files are rejected in staging.
- **`editable` (Write Scope):** Explicitly authorized for modification. Only files within the editable scope may be altered.

---

## 2.3 Autonomous Discovery

Developers no longer need to manually assemble file lists before asking questions:
1. When a task is submitted, the model inspects the Tree-sitter Repo Map.
2. If symbols or implementations are needed, the model emits protocol requests (e.g., `<context_request path="crates/core/src/context.rs" access="editable" />`).
3. The server adds requested files to the `Auto` context layer and immediately initiates the next round.
4. Once all needed files are loaded, the model produces code modifications.
