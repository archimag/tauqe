# 5. Keyboard-First Interface

TAUQE's terminal UI combines the design philosophies of **Magit** (structured, live document state) and **Org-mode** (collapsible outline hierarchy).

---

## 5.1 Primary Views and Universal Navigation

To guarantee universal compatibility across modern and legacy terminal emulators alike, all primary view modes and key actions support dual bindings (both modifier chords and standard function keys):

- **`F1` / `Alt+1` / `Ctrl+1` (Develop View):**
  - The central engineering cockpit and execution canvas where coding turns take place.
  - Displays turn progression as a structured, live document: collapsible reasoning blocks (`Ctrl+R`), streaming markdown explanations, and interactive proposed file diffs.
  - **Dual-Focus Architecture:** Switch focus seamlessly between the prompt editor and the response/diffs viewport using `Ctrl+Space` or `Shift+Tab`.
  - Multi-line intent editor with auto-growing height and Emacs navigation (`Ctrl+A`, `Ctrl+E`, `Alt+B`, `Alt+F`, `Ctrl+K`, `Ctrl+Y`, `Ctrl+W` / `Alt+Backspace` to kill word backward, `Tab` for 2-space indentation).
  - Persistent prompt history: Use `Ctrl+P` / `Ctrl+N` (or `Alt+P` / `Alt+N`) to cycle through previous prompt inputs. History is saved to `.tauqe/prompts.history` and persists across sessions.
  - Newline insertion: `Shift+Enter`, `Alt+Enter`, or `Ctrl+J`.
  - Response clipboard: Press `Alt+C` or `Alt+Y` while idle to copy the model's markdown response to the system clipboard.
  - Undo AI commit: Press `Ctrl+Z` or `Alt+U` to trigger safe rollback. If no AI commit exists, TAUQE displays a status notice without opening unnecessary dialogs.
  - Active LLM selection: Press `Alt+M` or `Ctrl+M` from any screen (or click the model in the header) to open the model picker. See section 5.7 for the tier model.
  - Contextual help: Press `Ctrl+H` / `Alt+H` from any view or `?` in navigation views to open the interactive help dialog.
  - Configuration reload: Press `Ctrl+O` to reload `tui.toml` settings without restarting TAUQE.

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
  - Unified navigation: `n` / `p`, `j` / `k`, or arrow keys.
  - Interactive findings checklist: fold/unfold items (`Tab` / `Space`), select items for Develop context (`x`), and copy findings (`c` / `y`).
  - Intentional status assignment: Press `t` to open the status selection dialog (`1` Todo, `2` Done, `3` Rejected).
- **`F4` / `Alt+4` / `Ctrl+4` (Plans View):**
  - Workspace for managing tactical task hierarchies and execution roadmaps.
  - Unified navigation: `n` / `p`, `j` / `k`, or arrow keys.
  - Interactive tree of tasks with statuses (`[ ]` Todo, `[▶]` InProgress, `[✓]` Done, `[−]` Cancelled).
  - Intentional status assignment: Press `t`, `s`, or `d` to open the status dialog (`1` Todo, `2` InProgress, `3` Done, `4` Cancelled).
  - Select items with `x` to inject them into the Develop prompt context as `<active_plan_context>`, fold/unfold branches (`Tab` / `Space`), switch between plans (`Tab` in plan selector), and copy markdown (`c` / `y`).
- **`F5` / `Alt+5` / `Ctrl+5` (History View):**
  - Paginated audit log of semantic turns, code review runs, AI commit hashes, and file modifications.
  - Collapsible items: All entries are folded by default, displaying the role badge, commit summary, and preview lines.
  - Press `Tab` or `Space` to fold/unfold the active entry and inspect the full response and modified files.
  - Item navigation & clipboard: Use `[` / `]` or `p` / `n` (`Alt+↑` / `Alt+↓`) to navigate turns, and press `c` / `y` (or `Enter`) to copy the selected item's text to the clipboard.
  - Header visibility guarantee: Active entry headers are automatically kept visible in the viewport during navigation.

---

## 5.2 Dual-Focus Architecture in Develop (Editor vs. Viewport)

In the Develop view, TAUQE resolves the tension between text editing and inspection through a clean two-focus model without modal traps:

### 1. Editor Focus (Default Mode)
- **Typing always types:** Every printable character is unconditionally inserted into the text buffer. Single-letter command keys are never intercepted during text entry.
- **Emacs navigation:** `Ctrl+A` / `Ctrl+E` for start/end of line, `Alt+B` / `Alt+F` for words backward/forward, `Ctrl+K` to kill to end of line, `Ctrl+Y` to yank from kill-ring, `Ctrl+W` / `Alt+Backspace` to kill word backward.
- **Persistent prompt history:** `Ctrl+P` and `Ctrl+N` cycle backward and forward through previous prompt inputs.
- **Switching focus:** Press `Ctrl+Space` or `Shift+Tab` to move focus to the response and diffs viewport.

### 2. Viewport Focus (Response & Diffs Inspection)
- **Visual indicator:** The prompt border dims to dark gray and displays `[VIEWPORT ACTIVE]`, while the currently inspected file shows an active cursor (`▶ `).
- **File selection:** Use `Ctrl+N` / `Ctrl+P` or arrow keys (`↑` / `↓`) to navigate through modified files. If no files are present, `Ctrl+N` and `Ctrl+P` scroll the assistant response.
- **Folding diffs:** Press `Tab`, `Enter`, or `Ctrl+T` to fold or unfold diff hunks for the active file.
- **Scrolling response:** `PgUp` and `PgDn` scroll the response content by full pages.
- **Zero-loss return to typing:** Pressing any printable character (letters, digits, symbols, or unshifted space) immediately returns focus to the prompt editor and inserts that character at the text cursor without dropping the key event.
- **Explicit return:** Press `Esc`, `Ctrl+Space`, or `Shift+Tab` to return focus to the prompt editor without modifying text.

---

## 5.3 Input Safety and Onboarding Protection

- **Commands require modifiers:** Destructive or state-changing actions use explicit modifiers (`Ctrl+Z` / `Alt+U` for Undo, `F6` / `Ctrl+S` for Squash, `Alt+C` for copying responses).
- **Draft protection on exit:** Pressing `Ctrl+Q` prompts for confirmation whenever a prompt draft is non-empty, a model turn is active, or an unapplied squash diff exists, preventing accidental loss of uncommitted work.
- **Onboarding isolation:** During initial configuration (`ViewMode::Onboarding`), view switching keys (`F1..F5`, `Ctrl+1..5`, `Alt+1..5`) and header mouse clicks are locked until setup is completed, preventing accidental bypass of credential verification.
- **Contextual help:** Press `Ctrl+H` / `Alt+H` from any view or `?` in navigation views to open the contextual help dialog.

---

## 5.4 Modal Dialog Architecture & Interaction Standard

Modal dialogs follow a unified, three-tier layout standard designed for keyboard reliability, safety, and layout neutrality:
- **Structure:** Every dialog consists of a distinct **Header** (title and context), a central **Body** (content, form, or options), and an **Actions / Keybindings Footer**.
- **Destructive Action Safety:** Confirmation dialogs (Quit with draft, Undo AI commit, Git squash, Cancel generation, Delete plan, Clear history) default focus securely to `[ Cancel ]`.
- **Button Navigation:** Switch focus between buttons using `Tab`, `Shift+Tab`, or arrow keys (`←`, `→`). Press `Enter` or `Space` to activate the focused button.
- **Fast Accelerators:** In confirmation dialogs, pressing `y` / `Y` confirms immediately, while `n` / `N`, `q`, or `Esc` cancels.
- **Intentional Status Dialogs:** In Plans and Review, task and finding statuses are selected through explicit modal dialogs rather than accidental cycling. Choose a target status using direct numeric keys (`1`..`4`), or navigate with `n` / `p` and confirm with `Enter` / `Space`.
- **Git Squash Safety:** Within the squash dialog (`F6` / `Ctrl+S`), `Enter` or `Space` folds/unfolds changed files in the file list, and `Enter` in the message editor inserts a newline. Triggering the squash requires `Ctrl+Enter` or `Ctrl+S`, which opens an explicit confirmation dialog summarizing affected commits.

---

## 5.5 User Configuration (`tui.toml`)

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

[theme]
# Color theme mode: "dark", "light", or "auto" (default: "auto").
# When "auto" is set, TAUQE checks COLORFGBG or terminal environment hints.
# Automatically detects 24-bit truecolor support and quantizes to ANSI-256 where required.
mode = "auto"

[notifications]
# Terminal audio bell (\x07 / BEL). In Kitty, WezTerm, Alacritty, and modern desktop
# window managers, this triggers window urgency hints or tab highlighting:
sound = true

# Native desktop notifications via terminal OSC sequences:
desktop = true

# Desktop notification protocol: "osc9" (default), "osc777", or "both":
desktop_protocol = "osc9"

# Only notify when the terminal window is unfocused:
only_unfocused = true

# Minimum turn/review execution duration in seconds before triggering notifications
# (prevents notification spam on fast sub-second interactions, default: 5):
min_duration_seconds = 5

# Optional shell command hook executed on long turn completion:
# command = "paplay /usr/share/sounds/freedesktop/stereo/complete.oga"
```

---

## 5.6 Long-Running Turn Notifications

Engineering tasks (discovery rounds, toolchain verifications, test suites) can take dozens of seconds. TAUQE ensures developers never miss turn completion when switching to another window or workspace:

- **Urgency Alert & Terminal Bell:** Emits a standard ASCII `BEL` (`\x07`), prompting modern terminal emulators (Kitty, WezTerm, iTerm2) to raise window urgency flags in your desktop taskbar or window manager.
- **Zero-Dependency Desktop Notifications:** Utilizes standard terminal OSC protocols (`OSC 777` and `OSC 9`) to display native desktop banners with turn summaries or verification statuses. Clicking the notification immediately focuses the terminal window.
- **Smart Duration Threshold:** Notifications fire only if turn execution equals or exceeds `min_duration_seconds` (default: 5 seconds), eliminating noise during rapid iterative dialogs.

---

## 5.7 Model Tiers and Model Selection

Tasks differ in difficulty and cost. A rename does not need the same model as a multi-file refactoring. Instead of making the developer remember model identifiers, TAUQE assigns models to three **roles** (tiers) in the `[models]` section of `tauqe.toml`:

```toml
[models]
junior = "~anthropic/claude-haiku-latest"      # cheap and fast
middle = "~google/gemini-flash-latest"         # default for most work
senior = "~anthropic/claude-sonnet-latest"     # complex, multi-file changes
default = "middle"                             # selection used at startup
auto_level_up = false                          # escalate on failure (plans and review only)
```

- **Selection model:** The active choice is either a tier (`Junior`, `Middle`, `Senior`) or one specific model. A tier is resolved to a concrete model by the server, so the TUI never guesses the mapping. If only some tiers are configured, the missing ones fall back to a configured neighbour.
- **Picker (`Alt+M` / `Ctrl+M`, or a click on the model in the header):** The first screen lists the tiers with their resolved models. Press `1`, `2`, or `3` to select a tier immediately, or `4` (`Others...`) to open the full catalog and pin one specific model. `Esc` closes the picker.
- **Header:** The model indicator shows the role and the resolved model (for example `Middle: ~google/gem…`). On narrow terminals only the role is shown. A pinned specific model is shown without a role.
- **Background work:** Squash commit messages and history compaction always use the Junior tier. They are routine tasks, so they should not spend the budget of a stronger model.
- **Level Up:** When `auto_level_up = true`, a plan step or review fix that still fails after verification healing is rolled back to the Git checkpoint taken before the step. The step then restarts on the next tier (Junior → Middle → Senior). Files found during discovery stay in context, so the stronger model does not search again. When the tiers are exhausted, or Level Up is off, the normal Fail-Fast stop applies. Develop never escalates automatically: the developer's explicit choice in the picker is always respected.
