# 5. Keyboard-First Interface

TAUQE's terminal UI combines the design philosophies of **Magit** (structured, live document state) and **Org-mode** (collapsible outline hierarchy).

---

## 5.1 Three Primary Views

- **`Ctrl+1` (Develop View):**
  - Displays the active conversation as a structured document: collapsible reasoning blocks (`Ctrl+R`), formatted markdown responses, and interactive proposed file edits.
  - Multi-line intent editor with auto-growing height and Emacs navigation (`Ctrl+A`, `Ctrl+E`, `Alt+B`, `Alt+F`, `Ctrl+K`, `Ctrl+Y`).
- **`Ctrl+2` (Context View):**
  - Displays Pinned, User, and Auto context layers with token metrics.
  - Quick keys: `e` (add editable), `r` (add read-only), `t` (toggle permission), `p` (promote auto to user), `c` (clear auto), `d` (remove file).
- **`Ctrl+3` (History View):**
  - Paginated audit log of semantic turns, AI commit hashes, and file modifications.
  - Dynamic loading on scroll and smart auto-scroll anchoring.

---

## 5.2 Interactive Edits & Folding

- Use `[` and `]` to navigate proposed files.
- Press `Tab` or `Space` to fold/unfold hunks and unified diffs.
- Press `u` with an empty prompt field to trigger immediate Undo.
- Press `?` at any time to open the contextual Help modal.
