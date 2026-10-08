# 5. Keyboard-First Interface

TAUQE's terminal UI combines the design philosophies of **Magit** (structured, live document state) and **Org-mode** (collapsible outline hierarchy).

---

## 5.1 Three Primary Views

- **`Ctrl+1` (Develop View):**
  - Displays the active conversation as a structured document: collapsible reasoning blocks (`Ctrl+R`), formatted markdown responses, and interactive proposed file edits.
  - Multi-line intent editor with auto-growing height and Emacs navigation (`Ctrl+A`, `Ctrl+E`, `Alt+B`, `Alt+F`, `Ctrl+K`, `Ctrl+Y`).
  - Copy response: Press `Alt+C` (or `c` / `y` with an empty prompt field) when the model is idle to copy the markdown response text to the system clipboard.
- **`Ctrl+2` (Context View):**
  - Displays Pinned, User, and Auto context layers with token metrics.
  - Quick keys: `e` (add editable), `r` (add read-only), `t` (toggle permission), `p` (promote auto to user), `c` (clear auto), `d` (remove file).
- **`Ctrl+3` (Review View):**
  - Dedicated code review workspace for static architectural and security audits.
  - Interactive findings checklist: fold/unfold items (`Tab` / `Space`), toggle status TODO/DONE/REJECTED (`t`), select items for Develop context (`x`), and copy findings (`c` / `y`).
- **`Ctrl+4` (History View):**
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
