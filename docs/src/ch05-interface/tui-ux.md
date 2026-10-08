# 5. Keyboard-First Interface

TAUQE's terminal UI combines the design philosophies of **Magit** (structured, live document state) and **Org-mode** (collapsible outline hierarchy).

---

## 5.1 Primary Views

- **`Ctrl+1` (Develop View):**
  - The central engineering cockpit and execution canvas where coding turns take place.
  - Displays the turn progression as a structured, live document: collapsible reasoning blocks (`Ctrl+R`), streaming markdown explanations, and interactive proposed file diffs.
  - Multi-line intent editor with auto-growing height and Emacs navigation (`Ctrl+A`, `Ctrl+E`, `Alt+B`, `Alt+F`, `Ctrl+K`, `Ctrl+Y`).
  - Response clipboard: Press `Alt+C` (or `c` / `y` when the prompt field is empty) while idle to copy the model's markdown response to the system clipboard.

### What Happens in Develop When You Submit a Prompt
When you submit an engineering intent, TAUQE does not simply invoke a single-shot completion. Instead, it guides the model through an **autonomous, multi-round bounded turn**:
1. **Exploration & Discovery Rounds:** The model analyzes the task against the Tree-sitter symbol map. If critical type definitions, helper modules, or project documentation are missing from context, it autonomously requests them via protocol tags (`<context_request>`, `<doc_request>`). The footer reflects the active discovery round (`Round 1`, `Round 2`...). Control does not bother the user with context logistics; the model gathers facts until the architectural picture is crystal clear.
2. **Code Generation & Virtual Staging:** Once equipped with the necessary context, the model drafts minimal, coherent changes (search/replace blocks, new files, renames). These edits are applied to an **in-memory virtual staging tree** first, ensuring zero disk corruption from speculative failures.
3. **Patch Retry Loop (Staging Resilience):** LLMs occasionally emit code hunks with slight indentation shifts, mismatched whitespace, or stale surrounding context lines that cannot be cleanly matched against disk. Instead of aborting the turn or dumping broken patches, TAUQE preserves all successfully staged files, pinpoints the failing files, and prompts the model with targeted patch error feedback to regenerate only the problematic hunks (up to `max_retries`).
4. **Deterministic Toolchain Verification & Self-Healing:** After in-memory staging converges and changes are written to disk, TAUQE automatically triggers deterministic project toolchains (`cargo check`, `npm test`, linters). If compile or lint errors are detected, the raw compiler diagnostics are packaged and fed back to the model for an immediate **verification healing cycle**, correcting syntax or type errors before the developer even inspects the code.
5. **Git Transaction & Rollback:** The entire successful sequence is recorded as an isolated Git commit. If the outcome deviates from the developer's intent, pressing `u` with an empty prompt immediately and atomically restores the repository to its exact pre-turn state.

- **`Ctrl+2` (Context View):**
  - Displays Pinned, User, and Auto context layers with token metrics.
  - Quick keys: `e` (add editable), `r` (add read-only), `t` (toggle permission), `p` (promote auto to user), `c` (clear auto), `d` (remove file).
- **`Ctrl+3` (Review View):**
  - Dedicated code review workspace for static architectural and security audits.
  - Interactive findings checklist: fold/unfold items (`Tab` / `Space`), toggle status TODO/DONE/REJECTED (`t`), select items for Develop context (`x`), and copy findings (`c` / `y`).
- **`Ctrl+4` (Plans View):**
  - Workspace for managing tactical task hierarchies and execution roadmaps.
  - Interactive tree of tasks with statuses (`[ ]` Todo, `[▶]` InProgress, `[✓]` Done, `[−]` Cancelled).
  - Select items with `x` to inject them into the Develop prompt context as `<active_plan_context>`, fold/unfold branches (`Tab` / `Space`), switch between plans (`Tab` in plan selector), and copy markdown (`c` / `y`).
- **`Ctrl+5` (History View):**
  - Paginated audit log of semantic turns, code review runs, AI commit hashes, and file modifications.
  - Collapsible items: All entries are folded by default, displaying the role badge, commit summary, and preview lines.
  - Press `Tab` or `Space` to fold/unfold the active entry and inspect the full response and modified files.
  - Item navigation & clipboard: Use `[` / `]` or `p` / `n` (`Alt+↑` / `Alt+↓`) to navigate turns, and press `c` / `y` (or `Enter`) to copy the selected item's text to the clipboard.
  - Header visibility guarantee: Active entry headers are automatically kept visible in the viewport during navigation.

---

## 5.2 Interactive Edits & Folding

- Use `[` and `]` to navigate proposed files.
- Press `Tab` or `Space` to fold/unfold hunks and unified diffs.
- Press `u` with an empty prompt field to trigger immediate Undo.
- Press `Alt+C` (or `c` on empty prompt) to copy the model's markdown response to clipboard.
- Press `?` at any time to open the contextual Help modal.
