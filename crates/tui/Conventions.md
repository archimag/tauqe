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

## 4. Single Source of Truth for Keyboard Commands

- **Automatic Help Generation:**
  - Every view mode must define its keyboard commands via a structured command registry.
  - The interactive Help system (`?`) must automatically derive its contents directly from these command definitions without manual duplication of keybindings in modal dialogs or popups.
