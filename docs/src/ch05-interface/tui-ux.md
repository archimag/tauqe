# 5. Keyboard-First Interface

TAUQE's terminal UI combines the design philosophies of **Magit** (structured, live document state) and **Org-mode** (collapsible outline hierarchy).

---

## 5.1 Primary Views and Universal Navigation

To guarantee universal compatibility across modern and legacy terminal emulators alike, all primary view modes and key actions support dual bindings (both modifier chords and standard function keys):

- **`F1` / `Alt+1` / `Ctrl+1` (Develop View):**
  - The central engineering cockpit and execution canvas where coding turns take place.
  - Displays the turn progression as a structured, live document: collapsible reasoning blocks (`Ctrl+R`), streaming markdown explanations, and interactive proposed file diffs.
  - Multi-line intent editor with auto-growing height and Emacs navigation (`Ctrl+A`, `Ctrl+E`, `Alt+B`, `Alt+F`, `Ctrl+K`, `Ctrl+Y`, `Ctrl+W` / `Alt+Backspace` to kill word backward, `Tab` for 2-space indentation).
  - Newline insertion: `Shift+Enter`, `Alt+Enter`, or `Ctrl+J`.
  - Response clipboard: Press `Alt+C` or `Alt+Y` while idle to copy the model's markdown response to the system clipboard.
  - Undo AI commit: Press `Ctrl+Z` or `Alt+U` to trigger safe rollback. If no AI commit exists, TAUQE displays a status notice without opening unnecessary dialogs.
  - Active LLM selection: Press `Alt+M` or `Ctrl+M` / `Ctrl+Y` from any screen to open the model selector.

### What Happens in Develop When You Submit a Prompt
When you submit an engineering intent, TAUQE does not simply invoke a single-shot completion. Instead, it guides the model through an **autonomous, multi-round bounded turn**:
1. **Exploration & Discovery Rounds:** The model analyzes the task against the Tree-sitter symbol map. If critical type definitions, helper modules, or project documentation are missing from context, it autonomously requests them via protocol tags (`<context_request>`, `<doc_request>`). The footer reflects the active discovery round (`Round 1`, `Round 2`...). Control does not bother the user with context logistics; the model gathers facts until the architectural picture is crystal clear.
2. **Code Generation & Virtual Staging:** Once equipped with the necessary context, the model drafts minimal, coherent changes (search/replace blocks, new files, renames). These edits are applied to an **in-memory virtual staging tree** first, ensuring zero disk corruption from speculative failures.
3. **Patch Retry Loop (Staging Resilience):** LLMs occasionally emit code hunks with slight indentation shifts, mismatched whitespace, or stale surrounding context lines that cannot be cleanly matched against disk. Instead of aborting the turn or dumping broken patches, TAUQE preserves all successfully staged files, pinpoints the failing files, and prompts the model with targeted patch error feedback to regenerate only the problematic hunks (up to `max_retries`).
4. **Deterministic Toolchain Verification & Self-Healing:** After in-memory staging converges and changes are written to disk, TAUQE automatically triggers deterministic project toolchains (`cargo check`, `npm test`, linters). If compile or lint errors are detected, the raw compiler diagnostics are packaged and fed back to the model for an immediate **verification healing cycle**, correcting syntax or type errors before the developer even inspects the code.
5. **Git Transaction & Rollback:** The entire successful sequence is recorded as an isolated Git commit. If the outcome deviates from the developer's intent, pressing `Ctrl+Z` or `Alt+U` opens a confirmation dialog to immediately and atomically restore the repository to its exact pre-turn state.

- **`F2` / `Alt+2` / `Ctrl+2` (Context View):**
  - Displays Pinned, User, and Auto context layers with token metrics.
  - Quick keys: `e` (add editable), `r` (add read-only), `t` (toggle permission), `p` (promote auto to user), `c` (clear auto), `d` (remove file).
- **`F3` / `Alt+3` / `Ctrl+3` (Review View):**
  - Dedicated code review workspace for static architectural and security audits.
  - Interactive findings checklist: fold/unfold items (`Tab` / `Space`), toggle status TODO/DONE/REJECTED (`t`), select items for Develop context (`x`), and copy findings (`c` / `y`).
- **`F4` / `Alt+4` / `Ctrl+4` (Plans View):**
  - Workspace for managing tactical task hierarchies and execution roadmaps.
  - Interactive tree of tasks with statuses (`[ ]` Todo, `[▶]` InProgress, `[✓]` Done, `[−]` Cancelled).
  - Select items with `x` to inject them into the Develop prompt context as `<active_plan_context>`, fold/unfold branches (`Tab` / `Space`), switch between plans (`Tab` in plan selector), and copy markdown (`c` / `y`).
- **`F5` / `Alt+5` / `Ctrl+5` (History View):**
  - Paginated audit log of semantic turns, code review runs, AI commit hashes, and file modifications.
  - Collapsible items: All entries are folded by default, displaying the role badge, commit summary, and preview lines.
  - Press `Tab` or `Space` to fold/unfold the active entry and inspect the full response and modified files.
  - Item navigation & clipboard: Use `[` / `]` or `p` / `n` (`Alt+↑` / `Alt+↓`) to navigate turns, and press `c` / `y` (or `Enter`) to copy the selected item's text to the clipboard.
  - Header visibility guarantee: Active entry headers are automatically kept visible in the viewport during navigation.

---

## 5.2 Zero Modal Ambiguity in Text Fields

To prevent accidental command invocation and cognitive traps, TAUQE enforces a strict input safety invariant:
- **Typing always types:** When the prompt editor or any text field is focused, every printable character is unconditionally inserted into the text buffer. Single-letter command shortcuts are never intercepted during text entry, even if the editor buffer is empty.
- **Commands require modifiers:** Operations in editing views use explicit modifiers (`Ctrl+Z` / `Alt+U` for Undo, `F6` / `Ctrl+S` for Squash, `Alt+C` for copying responses).
- **Diff file inspection:** Press `Alt+[` and `Alt+]` (or `↑` / `↓` when the prompt is empty) to navigate through proposed files, and `Tab` (when empty) or `Alt+Space` to fold or unfold the unified diff of the selected file.
- Press `?` at any time to open the contextual Help modal.

---

## 5.3 Layout-Agnostic Modal Dialogs

Confirmation dialogs for potentially destructive actions (Undo AI commit, Git history squash, cancel running generation, clear session history, delete plan) are designed to be completely independent of system keyboard layouts:
- **Interactive Button Focus:** Each confirmation dialog renders distinct `[ Confirm ]` and `[ Cancel ]` buttons, with the initial focus resting securely on **Cancel**.
- **Universal Navigation:** Developers can switch focus between buttons using `Tab`, `Shift+Tab`, or the arrow keys (`←`, `→`). Pressing `Enter` executes the currently focused action (safe cancellation by default).
- **Fast Shortcuts:** Pressing `y` or `Y` confirms immediately, while `Esc` or `n` / `N` cancels.
- **Git Squash Safety:** Within the squash dialog (`F6` / `Ctrl+S`), `Enter` or `Space` folds/unfolds changed files in the file list, and `Enter` in the message editor inserts a standard newline. Triggering the squash requires a dedicated shortcut (`Ctrl+Enter` or `Ctrl+S`), which prompts an explicit confirmation modal summarizing the commits and target base ref. Applying is strictly blocked while diffs are loading or AI commit messages are generating.

---

## 5.4 User Configuration (`tui.toml`)

In accordance with XDG standards, developer-specific terminal interface preferences are kept strictly separated from project configuration (`tauqe.toml`).

User settings reside in `$XDG_CONFIG_HOME/tauqe/tui.toml` (or `~/.config/tauqe/tui.toml`):

```toml
[input]
# Keyboard layout mapping preset:
#   "none"      - No character translation (default). Safe and layout-neutral.
#   "ru_jcuken" - Maps Cyrillic ЙЦУКЕН to QWERTY for navigation (j/k, x, t)
#                 and Ctrl/Alt shortcuts (Ctrl+B, Ctrl+Z, etc.).
layout = "none"

# Optional universal character mapping (langmap) for any non-Latin layout:
# Supports two formats:
# 1. Parallel string mapping: "from_chars;to_chars" (e.g. Vim langmap style)
# 2. Pairs list: "йq,цw,уe" or "й:q,ц:w"
# langmap = "йцукенгшщзхъ;qwertyuiop[]"

[notifications]
# Terminal audio bell (\x07 / BEL). In Kitty, WezTerm, Alacritty, and modern desktop
# window managers, this triggers window urgency hints or tab highlighting:
sound = true

# Native desktop notifications via terminal OSC sequences (OSC 777 and OSC 9):
# Supported natively by Kitty, Ghostty, WezTerm, iTerm2, foot, and Windows Terminal.
# Delivers notifications directly to your desktop environment without external dependencies.
desktop = true

# Minimum turn/review execution duration in seconds before triggering notifications
# (prevents notification spam on fast sub-second interactions, default: 5):
min_duration_seconds = 5

# Optional shell command hook executed on long turn completion:
# command = "paplay /usr/share/sounds/freedesktop/stereo/complete.oga"
```

---

## 5.5 Long-Running Turn Notifications

Engineering tasks (discovery rounds, toolchain verifications, test suites) can take dozens of seconds. TAUQE ensures developers never miss turn completion when switching to another window or workspace:

- **Urgency Alert & Terminal Bell:** Emits a standard ASCII `BEL` (`\x07`), prompting modern terminal emulators (Kitty, WezTerm, iTerm2) to raise window urgency flags in your desktop taskbar or window manager.
- **Zero-Dependency Desktop Notifications:** Utilizes standard terminal OSC protocols (`OSC 777` and `OSC 9`) to display native desktop banners with turn summaries or verification statuses. Clicking the notification immediately focuses the terminal window.
- **Smart Duration Threshold:** Notifications fire only if turn execution equals or exceeds `min_duration_seconds` (default: 5 seconds), eliminating noise during rapid iterative dialogs.
