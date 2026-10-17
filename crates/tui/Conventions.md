# Terminal UI Conventions

This document specifies mandatory architectural and UX invariants for the Tauqe Terminal User Interface (`crates/tui`).

---

## 1. Layout Semantics: Header vs Footer

1. **Header is for Actions:**
   - The top header is reserved conceptually for navigation, mode switching, and direct user actions.
   - Dynamic session telemetry and background status feedback belong in the footer, not in the header.

2. **Footer is for Indicators:**
   - The bottom footer is reserved conceptually for operational telemetry, indicators, and status feedback.
   - Navigation controls and interactive workflow triggers belong in the header or dedicated view panes, not in the footer.

---

## 2. Tab Hierarchy and History Invariant

- **History is always the last tab:**
  - The History tab represents the chronological session audit log and must remain the final tab in the navigation order.
  - Any newly introduced view modes or tabs must be positioned before the History tab.

---

## 3. Single Source of Truth for Keyboard Commands and Clean Views

1. **Typed Command Specifications (`CommandSpec`):**
   - Every keyboard binding is declared exactly once, as an executable command specification in `crates/tui/src/command.rs`: an abstract [`KeyChord`], a typed action, a [`CommandCategory`], and a help description.
   - The input dispatcher that intercepts keys and the Help generator that documents them read the same structures, so the keys the developer sees in Help can never drift from the keys the application actually intercepts.
   - Scattered string descriptions and hardcoded key checks in handlers are forbidden. A view owns a static [`CommandMap`] of its specifications and derives both input resolution and Help rendering from it.
   - Maps are grouped by scope: `GLOBAL_COMMAND_MAP` (application-wide), `EDITOR_COMMAND_MAP` (the shared text editor), per-view maps, and the shared `DIALOG_COMMAND_MAP` for modal dialogs.

2. **Semantic Key Chords (`KeyChord`):**
   - An abstract chord names intent independently of the physical keyboard, so one specification describes the developer's actual modifiers.
   - `Primary` (`C-`) and `Meta` (`M-`) are the two symmetric command modifiers, resolved dynamically through `InputConfig::is_primary(...)` / `InputConfig::is_meta(...)`. Hardcoding `KeyModifiers::CONTROL` or `KeyModifiers::ALT` in a view is forbidden.
   - `PrimaryOrMeta` binds a chord reachable on either command modifier, so a terminal without a dedicated Meta key still reaches the action.
   - `Bare` names a single alphanumeric or navigation key without command modifiers. Bare chords are active only in structural navigation and are unconditionally blocked while a text editor is capturing keystrokes, so the unconditional character insertion invariant (§5) always holds.
   - `Shift` names an explicit `Shift` combination (`Shift+Tab`, `Shift+C`, `Shift+X`) used for reverse motion, destructive actions, and batch operations, deliberately separated from typed characters and command modifiers.
   - `Ctrl` and `Alt` name raw physical chords whose key must never be remapped (the `C-g` keyboard-quit and the `Ctrl+C` cancel chord).
   - Chord display is projected through the configured modifiers, so Help and dialog footers always name the real keys instead of the abstract notation.
   - The zero F-keys invariant (§5) holds across every map: `KeyCode::F(...)` is never bound.

3. **Prohibition of Action Hint Bars in Primary Views:**
   - Primary view modes (Develop, Context, Review, Plans, History) must **never** render persistent keyboard command toolbars, shortcut hint banners ("Actions: Tab Fold, x Check..."), or bottom action bars.
   - The standardized Help modal (`Ctrl+H` / `Alt+H`, or `?` in navigation views) is the single authoritative mechanism for discovering view commands.
   - Bottom status areas are reserved strictly for operational telemetry, entity metadata, and progress indicators.
   - Keybinding hints are permissible only inside temporary, contextual modal dialogs (confirmations, selection popups) where immediate contextual guidance is required.

4. **Three-Tier Cognitive Classification of Help Commands:**
   - The Help modal derives its contents strictly from the command maps without manual duplication, and ranks commands by cognitive importance through the [`CommandCategory`] model:
     1. **Essential** — costly operations that spend tokens or touch Git (sending and cancelling turns, executing a step or batch, squashing commits).
     2. **Interface** — presentation, navigation, and view state (switching views, tree navigation, folding, focus toggling).
     3. **Helpers** — auxiliary operations that do not mutate interface state (copying to the clipboard, filtering, recentering).
   - Categories keep their canonical order (Essential → Interface → Helpers) and any empty category is omitted. Arbitrary or chronological insertion is forbidden: each command belongs to the tier matching its daily cognitive weight, not the order in which it was implemented.

5. **Decoupled Three-Tab Help Subsystem (Global, Current Mode, Text Editor):**
   - The Help modal presents three dedicated tabs — Global Shortcuts, Current Mode Shortcuts, and Text Editor Help — completely separating application-wide navigation from view-specific workflows and the shared text editor vocabulary.
   - Global shortcuts are derived from `GLOBAL_COMMAND_MAP`; active view shortcuts are derived from the mode's typed `CommandMap` (categorized into Essential, Interface, Helpers); and Readline/Emacs text editor bindings are derived from `EDITOR_COMMAND_MAP`.
   - Separating Global and Current Mode ensures neither surface overflows the screen or buries mode-specific commands under application-wide shortcuts.

6. **Unified Dialog Footers from the Local Command Map:**
   - Modal footer hints are not hand-laid per dialog. They are generated by the reusable modal component from the shared `DIALOG_COMMAND_MAP`, with the configured `C-` / `M-` labels applied automatically.
   - The footer therefore advertises exactly the chords the dialogs intercept, deduplicated and grouped (focus / activate / fast select / dismiss), and stays consistent across every modal.
   - The hint is rendered as a docked, `palette.overlay`-filled help panel below the action bar (see §6.4), with its chords, verbs, and separators styled through semantic text roles.

---

## 4. Hierarchical Tree Navigation and Org-Mode Paradigm

- **Hierarchical Tree Structures (Plans, Review):**
  - Implement the conceptual Org-mode paradigm for structured trees without burdening navigation with complex Emacs-specific modifier chords.
  - **Visibility Cycling:** Support clear local folding on items (`Tab` or `Space`) and global folding (`a`) to toggle between dense overview (all collapsed) and expanded states.
  - **Orthogonal Status and Execution:** Task lifecycle statuses (`TODO`, `IN_PROGRESS`, `DONE`, `CANCELLED`, consciously chosen via modal dialog `t`) are strictly separated from execution (`x`).
  - **High Information Density (No Decorative Gaps):** Collapsed tree items must be rendered tightly adjacent without artificial blank gaps or trailing empty lines. Expanded items with markdown details must be cleanly separated by exactly one line.
  - **Simple Dedicated Navigation:** In view modes without text input (Plans, Review, Context), prioritize simple direct keys (`n`/`p`, `↑`/`↓`, `x` for execute, `c` for copy, `s` for status filter, `t` for status dialog) rather than multi-key chord sequences.

---

## 5. Unified Emacs & Magit Paradigm: Direct Structural Navigation vs Readline Editing

1. **Two Distinct Interaction Modes:**
   - **Magit / Org-Mode Structural Navigation (Non-Input Contexts):**
     In all views, dialogs, and panels where a text buffer is NOT actively receiving text (Plans, Review, History, Context file list, dialog button toggling):
     - Canonical navigation uses simple, direct keys: `n` (*next*) and `p` (*previous*), following the Magit/Dired standard. Arrows `↑`/`↓` are preserved as secondary aliases.
     - Actions use direct bare keys (`x` for execute step/session, `t` for status dialog, `s` for filter, `c`/`y` for copy, `Tab`/`Space` for folding, `Enter`, `Esc`, `q` to return/bury buffer, `?` for help).
     - Modifier chords are never required for basic browsing, folding, or status toggling.
     - Never intercept these single keys when a text editor becomes active.
   - **Emacs-Style Commands with Zero Modal Ambiguity (Text-Input Contexts):**
     In all views and dialogs where a text input editor is active (Develop prompt input, Squash message editor, file adding, modal text fields):
     - **Unconditional Character Insertion:** Every printable character typed without a command modifier MUST be unconditionally inserted into the text buffer. Intercepting bare alphanumeric keys (such as `u`, `s`, `c`, `y`, `[`, `]`, or `Space`) when the prompt buffer is empty is strictly forbidden.
     - All functional commands and actions within text input contexts MUST require an explicit command modifier.

2. **Symmetric Abstract Command Modifiers (`C-` and `M-`):**
   - Commands in text input contexts are formulated conceptually with two independent Emacs-style chords: the primary modifier `C-<key>` and the meta modifier `M-<key>`.
   - `C-` is the abstract Primary Command Modifier, configured in `$XDG_CONFIG_HOME/tauqe/tui.toml` under `[input] primary_modifier = "ctrl"` (default) or `"alt"`. `M-` is the abstract Meta Command Modifier, configured under `[input] meta_modifier = "alt"` (default) or `"ctrl"`.
   - The two modifiers are independent and symmetric, so either chord can be remapped and a terminal that lacks a dedicated Meta key still reaches every action.
   - The codebase must never hardcode raw `KeyModifiers::CONTROL` or `KeyModifiers::ALT` checks for input-level actions; it must resolve them through `InputConfig::is_primary(...)` and `InputConfig::is_meta(...)`.
   - The AltGr / compose chord (`Ctrl+Alt`) belongs to neither modifier, so typing composed characters or `AltGr` digits never triggers an application command.
   - Standard bindings:
     - `C-Enter` — send prompt / confirm message editor;
     - `C-Z` — undo last AI commit;
     - `C-S` / `M-S` — open squash commits dialog;
     - `C-W` — kill word backward;
     - `M-B`, `M-F` — move word backward / forward;
     - `C-A`, `C-E`, `C-K`, `C-U`, `C-Y`, `C-D`, `C-B`, `C-F` — standard line editing and yank;
     - `C-P`, `C-N` — move cursor line up / down;
     - `M-P`, `M-N` — navigate prompt history backward / forward;
     - `C-T` — toggle file diff folding;
     - `C-Space` / `Shift+Tab` — switch Develop focus between the prompt and the Viewport;
     - `C-J` — insert newline;
     - `C-1` … `C-5` / `M-1` … `M-5` — switch tabs;
     - `C-M` / `M-M` — model selection picker;
     - `C-C` — cancel generation;
     - `C-G` — keyboard-quit (Emacs `keyboard-quit`): the canonical abort chord, normalized globally to `Esc` before any handler runs, so it aborts the active operation, dismisses the top-most modal, returns from an auxiliary view, or detaches the active discussion focus exactly as `Esc` would;
     - `C-L` — recenter the active item or viewport (Emacs `recenter-top-bottom`);
     - `C-R` — toggle reasoning block;
     - `C-O` — reload configuration;
     - `C-H` / `M-H` — open the contextual Help dialog;
     - `M-T` — open the color theme selector;
     - `C-[` / `C-]` — navigate response file diffs;

3. **Zero F-Keys Invariant and Universal Fallbacks:**
   - The TUI must never bind `KeyCode::F(...)` (`F1`..`F12`). Function keys are intercepted unpredictably by window managers, terminal multiplexers, and laptop firmware, some are reserved by the terminal itself, and 60% and compact keyboards omit the row entirely. Every action must be reachable through a bare key, `C-`, `M-`, or a dedicated navigation key.
   - Universal navigation keys (dedicated arrows, `Home`/`End`, `PgUp`/`PgDn`) remain active across all terminal types, regardless of keyboard protocol capabilities.
   - `Alt+Enter` and `Ctrl+J` serve as universal fallbacks for newline insertion in legacy terminals lacking `Shift+Enter`.

4. **Explicit Confirmation for Destructive and Long-Running Actions:**
   - Any modal dialog that initiates a destructive action (such as `git undo`, delete plan, clear history, or cancel generation) or a long-running, autonomous execution workflow (such as executing a plan step, plan batch, or review finding batch) MUST feature interactive `[ Confirm ] / [ Cancel ]` buttons with default focus on safe cancellation (`Cancel`).
   - Pressing `Enter` or `Esc` defaults to safe cancellation.
   - Quick confirmation is permitted via direct `y` or `Y` keypress.
   - Actions like `git undo` must verify that an undoable entity exists before prompting the user, emitting an informative notification otherwise.

5. **Canonical Keyboard-Quit (`C-g`):**
   - `C-g` (ASCII BEL, `0x07`) is the universal abort chord inherited from Emacs. It is normalized once, before any view or modal handler runs, to `Esc`, so every surface reacts identically without a per-handler binding.
   - The chord must never be implemented by installing a `SIGINT` handler. Normalizing the key event keeps the process free of signal handling and preserves the terminal's standard `Ctrl+C` behavior.
   - Because `C-g` resolves to `Esc`, its effect is always `Esc`'s effect in the active context: interrupt a running generation, dismiss the top-most modal, return from an auxiliary view, or detach the active discussion focus. It never clears history; history clearing is the dedicated `Shift+C` binding inside the History view.

---

## 6. Modal Dialog Architecture & Interaction Standard

1. **Three-Tier Dialog Structure (Header, Body, Actions):**
   - Every modal dialog must follow a consistent three-tier layout:
     - **Header / Title:** Border title (`Block::bordered().title(...)`) indicating operation context and status (e.g. ` Confirm Cancellation `, ` Select Model `, ` Squash AI Commits (Applying...) `).
     - **Body:** Central functional area (formatted text/alert for confirmations, virtual scrollable list for selectors, or structured input/diff pane).
     - **Actions Footer:** Dedicated bottom action bar separated by visual hierarchy or divider, presenting explicit action buttons (`[ Confirm ]`, `[ Cancel ]`, etc.).
     - **Docked Help Panel:** Immediately below the action bar, a non-interactive hint strip docked to the bottom border and rendered as a `palette.overlay`-filled row (see §6.4). It is chrome, not a fourth content tier.

2. **Standardized Command Navigation:**
   - **Focus Cycling:** `Tab` and `Shift+Tab` cycle focus between actionable controls (buttons or fields).
   - **Horizontal Button Stepping:** `←` / `→` move focus between adjacent buttons in action bars.
   - **Execution & Safe Defaults:** `Enter` or `Space` activates the currently focused button. For destructive actions as well as long-running, autonomous execution tasks (plan execution, review finding batch execution), safe cancellation (`Cancel`) must always receive default focus.
   - **Fast Accelerators in Confirmation Modals:** Direct `y` / `Y` confirms immediately; `n` / `N` or `Esc` cancels and closes immediately, regardless of button focus.
   - **Dismissal:** `Esc` or `q` unconditionally cancels and dismisses dialogs (except where a text input editor is actively capturing keystrokes).
   - **List Navigation in Selection Dialogs:** When the body contains a selectable list (e.g., model picker), items are navigated using standard keys (`n`/`p`, `j`/`k`, `↑`/`↓`, `PgUp`/`PgDn`, `Home`/`End`), preserving current item selection when focus shifts to action buttons.

3. **Batch Cancellation Dialog (Three-Way Stop):**
   - When a running batch (plan or review execution) receives a cancellation request, the modal presents three explicit actions instead of a binary confirm: `Continue`, `Soft Stop`, and `Hard Stop`. A stop is thereby a deliberate choice rather than an accidental dismissal.
   - `Soft Stop` lets the current member finish its bounded turn, compiler verification, and Git commit before the queue halts; `Hard Stop` aborts the current member immediately. `Continue` keeps the queue running and holds the safe default focus, so an accidental keypress never discards committed work.
   - Direct accelerators `c` (Continue), `s` (Soft Stop), and `h` (Hard Stop) select an action without moving focus; `Tab`, `←`/`→`, `Enter`/`Space`, and `Esc` follow the standard dialog navigation rules above.
   - While a soft stop is pending, the footer indicates it (e.g. `[Stopping after current step]`), so the developer can distinguish a graceful drain still in progress from an immediate abort (see §1).

4. **Docked Help Panel and Layering Invariant:**
   - The hint text of a modal dialog is never a loose caption hanging in the gap between the buttons and the border. It forms a **docked help panel**: a full-width strip filled with `palette.overlay` and docked to the bottom inner row of the dialog, immediately above the bottom border.
   - Docking the hint to its own filled row separates persistent chrome from the interactive action bar and visually closes the dialog card, so the eye reads the buttons as controls and the strip as guidance. A transparent or unstyled hint row is forbidden (see §8).
   - The panel is non-interactive: it never receives focus, is never hit-tested for activation, and exposes no selectable controls. Its text is generated from the shared `DIALOG_COMMAND_MAP` (§3.6), never hand-authored per dialog.

5. **Semantic Typography for Hint Chords:**
   - Hint text is not a flat string; it is assembled from structured `Span`s so every token carries its own semantic role:
     - key chords render in `palette.text_primary` (emphasized);
     - action verbs render in `palette.text_secondary`;
     - separator glyphs (`/` and `•`) recede into `palette.text_muted`.
   - This semantic contrast keeps the keys scannable and legible on any terminal or color profile and honors the absolute prohibition of raw colors (§8). Hints are projected through the configured `C-` / `M-` modifiers before styling, so the styled tokens always name the real keys.

6. **Mouse Hit-Testing Synchronization Invariant:**
   - Every geometry helper that reports clickable button rectangles (for example `modal_action_buttons`) must compute its coordinates from the exact same `Layout` formula the render path uses. Shifting the action bar, or inserting the docked help panel below it, must therefore never desynchronize the click targets from the drawn buttons.
   - Rendering and hit-testing share a single geometry source of truth, so the confirm/cancel buttons that are drawn are always the buttons that are clickable.

---

## 7. Develop Focus Model (Prompt vs Viewport)

1. **Two Focus Targets:**
  - The Develop tab has two focus targets: the prompt editor (default) and the Viewport (model response and file diffs).
  - `C-Space` and `Shift+Tab` switch focus between them. `Esc` in the Viewport returns focus to the prompt.
  - Reason: arrow keys and `C-P` / `C-N` have different meanings in the editor (cursor navigation) and in the response (file selection, scrolling). Separate focus removes this conflict without adding bare-letter shortcuts.

2. **Viewport Navigation:**
  - `C-N` / `C-P` and `↓` / `↑` select the next / previous modified file, or scroll the response when there are no files.
  - `Tab`, `Enter`, and `C-T` fold or unfold the selected diff.
  - `PgUp`, `PgDn`, `Home`, and `End` scroll the response.
  - Bare letters and `Space` are never Viewport commands. This keeps the unconditional character insertion rule from §5.

3. **Automatic Return to the Prompt:**
  - Any key that types text or edits the prompt moves focus back to the prompt and is applied there. No keystroke is lost.
  - Sending a prompt also returns focus to the prompt.

4. **Focus Indicator:**
  - While the Viewport has focus, the prompt border is dimmed and titled `[VIEWPORT]`.

---

## 8. Semantic Design System and Absolute Prohibition of Raw Colors

1. **Single Source of Truth for Visual Styling (`ThemeEngine` & `ThemePalette`):**
   - All presentation rendering across primary views, modal dialogs, popup overlays, text editors, and global chrome must resolve colors and styles exclusively through `crate::theme::engine::current_engine()` (or a passed `&ThemeEngine`) and the semantic `ThemePalette` tokens.
   - UI components must never hardcode concrete terminal colors, construct ad-hoc ANSI styles, or invent local color constants.

2. **Absolute Prohibition of Raw Colors:**
   - Direct usage of raw `ratatui::style::Color` variants (e.g. `Color::Black`, `Color::White`, `Color::Red`, `Color::Green`, `Color::Yellow`, `Color::Blue`, `Color::Magenta`, `Color::Cyan`, `Color::Gray`, `Color::DarkGray`, `Color::Light*`, `Color::Rgb(...)`, `Color::Indexed(...)`) or unstyled/default-filled blocks in presentation code is **strictly forbidden**.
   - The sole allowed color constructor in UI code is projection through `engine.color(role)`, `engine.fg(role)`, and `engine.bg(role)`.

3. **Mandatory Surface and Background Layering:**
   - Visual hierarchy must be maintained strictly through palette layer roles:
     - `canvas` — root workspace/window background;
     - `surface` — modal dialog cards, popups, and bordered containers;
     - `overlay` — floating dropdowns, selection rows, and contextual menus;
     - `code_bg` — inline and block code surfaces.
   - Leaving modal dialogs, popups, or confirmation overlays with transparent, unstyled, or default terminal backgrounds (`Color::Reset` or unstyled blocks) is strictly prohibited. Every modal container MUST explicitly fill its background using `palette.surface` (or `palette.overlay`) so underlying content is visually occluded across all color depths.

4. **Semantic Role Mapping:**
   - **Text:** `text_primary` for headings and high-priority content, `text_secondary` for descriptive labels and secondary values, `text_muted` for subtle hints, borders, and inactive separators, `text_inverse` for text rendered over accented backgrounds (active buttons, badges).
   - **Borders & Chrome:** `border` for standard container dividers; `ModalTone` (`Info`, `Warning`, `Dangerous`, `Accent`) for semantic dialog border accents.
   - **Action Controls:** `button_bg` and `button_fg` for resting buttons, `button_active_bg` and `text_inverse` for focused/active buttons, `status_error` and `text_inverse` for dangerous/destructive action buttons.
   - **Status & Indicators:** `status_info`, `status_success`, `status_warning`, `status_error` for notifications, telemetry, and lifecycle badges.
   - **Diffs & Patches:** `diff_added`, `diff_removed`, `diff_context` for unified diffs and file reviews.

5. **Projection Transparency:**
   - View code must never attempt manual ANSI downsampling, 16-color degradation branching, or separate light/dark logic branches.
   - The `ThemeEngine` automatically projects semantic tokens through the active `ColorProjector` (`Truecolor` → `ANSI 256` → `ANSI 16` fallback with attributes), гарантируя legibility and WCAG contrast in all terminal environments.

---

## 9. Zero-Hardcode User-Facing Text Invariant (100% Localized Presentation)

1. **Single Source of Localized Text (`i18n.t` and `active_t`):**
   - Every user-facing string rendered by the TUI (tab titles, modal dialog titles and bodies, button labels, key descriptions in `CommandSpec`, bounded-turn lifecycle badges, status messages, notifications `notify_*`, confirmation alerts) must be resolved dynamically through `i18n.t("key")` (or the process-global snapshot `active_t("key")`).
   - Hardcoding raw user-facing string literals in `Span::raw("...")`, `Line::from("...")`, `Paragraph::new("...")`, or `Button::new("...")` is strictly prohibited and classified as an architectural defect.

2. **Canonical Catalog as Embedded SSOT (`canonical_catalog`):**
   - Every user-facing text key must be declared in `crates/tui/src/i18n/catalog.rs` inside `canonical_catalog()`.
   - Each entry defines:
     - `key` — stable dot-notated identifier (`category.item`);
     - `english` — authoritative English source text;
     - `context` — semantic usage description for the LLM translation engine (e.g. "confirmation button in modal dialog", "help description for cancelling current turn");
     - `max_width` — explicit physical terminal column budget (`Some(N)`) for buttons, badges, tabs, and columns; or `None` (unbounded) for documentation, command descriptions, and multi-line explanations.

3. **Budgeted vs Unbounded Text Geometry:**
   - Elements constrained by fixed grid layouts (`Constraint::Length`) must specify a realistic `max_width` budget to prevent terminal grid overflow.
   - Elements rendered within scrollable or wrapping views (Help dialog, command lists, multi-line explanations) must use `max_width = None` so translations remain natural and uncompromised.

4. **Allowed Non-Localized Exceptions (Technical Artifacts):**
   - Raw string literals are permissible exclusively for:
     - LLM generation output and streaming response payloads (markdown, code blocks, diff hunks);
     - File system paths, Git commit hashes, branch names, and repository metadata;
     - Model and provider identifiers (e.g. `anthropic/claude-3.7-sonnet`);
     - Technical chrome and non-verbal syntax separators (`/`, `•`, ` : `, `[+]`, `[-]`, `...`, borders);
     - Key chord notation rendered by `KeyChord::display()` (e.g. `C-`, `M-`, `Tab`).
