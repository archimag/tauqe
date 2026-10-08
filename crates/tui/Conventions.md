# Terminal UI Conventions

This document specifies mandatory architectural and UX invariants for the Tauqe Terminal User Interface (`crates/tui`).

---

## 1. Context Requirement

Whenever modifying or extending code within `crates/tui/`, this conventions document must be included in the context and strictly observed.

---

## 2. Layout Semantics: Header vs Footer

1. **Header is for Actions:**
   - The top header is reserved conceptually for navigation, mode switching, and direct user actions.
   - Dynamic session telemetry and background status feedback belong in the footer, not in the header.

2. **Footer is for Indicators:**
   - The bottom footer is reserved conceptually for operational telemetry, indicators, and status feedback.
   - Navigation controls and interactive workflow triggers belong in the header or dedicated view panes, not in the footer.

---

## 3. Tab Hierarchy and History Invariant

- **History is always the last tab:**
  - The History tab represents the chronological session audit log and must remain the final tab in the navigation order.
  - Any newly introduced view modes or tabs must be positioned before the History tab.

---

## 4. Single Source of Truth for Keyboard Commands and Clean Views

1. **Automatic Help Generation:**
   - Every view mode must define its keyboard commands via a structured command registry.
   - The interactive Help system (`?`) must automatically derive its contents directly from these command definitions without manual duplication of keybindings in modal dialogs or popups.

2. **Prohibition of Action Hint Bars in Primary Views:**
   - Primary view modes (Develop, Context, Review, Plans, History) must **never** render persistent keyboard command toolbars, shortcut hint banners ("Actions: Tab Fold, x Check..."), or bottom action bars.
   - The standardized Help modal (`?`) is the single authoritative mechanism for discovering view commands.
   - Bottom status areas are reserved strictly for operational telemetry, entity metadata, and progress indicators.
   - Keybinding hints are permissible only inside temporary, contextual modal dialogs (confirmations, selection popups) where immediate contextual guidance is required.

---

## 5. Hierarchical Tree Navigation and Org-Mode Paradigm

- **Hierarchical Tree Structures (Plans, Review):**
  - Implement the conceptual Org-mode paradigm for structured trees without burdening navigation with complex Emacs-specific modifier chords.
  - **Visibility Cycling:** Support clear local folding on items (`Tab` or `Space`) and global folding (`a`) to toggle between dense overview (all collapsed) and expanded states.
  - **Orthogonal Status and Focus:** Task lifecycle statuses (`TODO` → `IN_PROGRESS` → `DONE` → `CANCELLED`, cycled via `t`) are strictly separated from context inclusion (`[x]`, toggled via `x`).
  - **High Information Density (No Decorative Gaps):** Collapsed tree items must be rendered tightly adjacent without artificial blank gaps or trailing empty lines. Expanded items with markdown details must be cleanly separated by exactly one line.
  - **Simple Dedicated Navigation:** In view modes without text input (Plans, Review), prioritize simple direct keys (`j`/`k`, `↑`/`↓`, `c` for copy, `s` for status filter) rather than multi-key chord sequences.

---

## 6. Dual Keyboard Paradigm: Vim-Style Navigation vs Emacs-Style Text Commands

1. **Two Distinct Interaction Modes:**
   - **Vim-Style Navigation (Non-Input Contexts):**
     In all views, dialogs, and panels where a text buffer is NOT actively receiving text (Plans, Review, History, Context file list, dialog button toggling):
     - Navigation and actions use simple, direct bare keys (`j`/`k`, `x`, `t`, `s`, `c`, `Tab`, `Space`, `Enter`, `Esc`, `q`, `?`).
     - Modifier chords are never required for basic browsing, folding, or status toggling.
     - Never intercept these single keys when a text editor becomes active.
   - **Emacs-Style Commands with Zero Modal Ambiguity (Text-Input Contexts):**
     In all views and dialogs where a text input editor is active (Develop prompt input, Squash message editor, file adding, modal text fields):
     - **Unconditional Character Insertion:** Every printable character typed without a command modifier MUST be unconditionally inserted into the text buffer. Intercepting bare alphanumeric keys (such as `u`, `s`, `c`, `y`, `[`, `]`, or `Space`) when the prompt buffer is empty is strictly forbidden.
     - All functional commands and actions within text input contexts MUST require an explicit command modifier.

2. **The Abstract Primary Command Modifier (`C-`):**
   - Commands in text input contexts are formulated conceptually as `C-<key>` (in Emacs tradition).
   - `C-` represents the abstract Primary Command Modifier, configured by the user in `$XDG_CONFIG_HOME/tauqe/tui.toml` under `[input] primary_modifier = "ctrl"` (default) or `"alt"`.
   - The codebase must never hardcode raw `KeyModifiers::CONTROL` checks for input-level actions without checking `primary_modifier.matches(...)`.
   - Standard `C-` bindings:
     - `C-Enter` — send prompt / confirm message editor;
     - `C-Z` — undo last AI commit;
     - `C-S` — open squash commits dialog;
     - `C-W` — kill word backward;
     - `C-A`, `C-E`, `C-K`, `C-U`, `C-Y`, `C-D`, `C-B`, `C-F` — standard line editing and yank;
     - `C-J` — insert newline;
     - `C-1` … `C-5` — switch tabs;
     - `C-M` / `C-Y` — model selection picker;
     - `C-C` — cancel generation;
     - `C-L` — clear history;
     - `C-R` — toggle reasoning block;
     - `C-O` — reload configuration;
     - `C-[` / `C-]` — navigate response file diffs;
     - `C-Space` — toggle file diff folding.

3. **Universal Hardware / Terminal Fallbacks:**
   - Universal function keys (`F1`..`F5` for tabs, `F6` for squash, dedicated arrows, `Home`/`End`, `PgUp`/`PgDn`) remain active across all terminal types, regardless of keyboard protocol capabilities.
   - `Alt+Enter` and `Ctrl+J` serve as universal fallbacks for newline insertion in legacy terminals lacking `Shift+Enter`.

4. **Explicit Confirmation for Destructive Actions:**
   - Destructive confirmation modals (such as `git undo`, delete plan, clear history, or cancel generation) must feature interactive `[ Confirm ] / [ Cancel ]` buttons with default focus on safe cancellation (`Cancel`).
   - Pressing `Enter` or `Esc` defaults to safe cancellation.
   - Quick confirmation is permitted via direct `y` or `Y` keypress.
   - Actions like `git undo` must verify that an undoable entity exists before prompting the user, emitting an informative notification otherwise.
