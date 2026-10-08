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
